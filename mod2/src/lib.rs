mod store;
mod trace;

#[cfg(test)]
mod smoke_test;
#[cfg(test)]
mod bench;

use aviutl2::{
    module::{ScriptModuleCallHandle, ScriptModuleFunctions, ScriptModuleParamTable},
    AnyResult,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use store::{Cached, SourceStamp, STORE};
use trace::{content_key, rasterize_svg, vectorize_image, vectorize_rgba, TraceConfig, TraceResult};

/// 状態はモジュール全体の `store::STORE` に持つ（aux2 が `svg_trace_copy_svg_v1` で同じものを読むため）
#[aviutl2::plugin(ScriptModule)]
struct SvgTraceModule;

impl aviutl2::module::ScriptModule for SvgTraceModule {
    fn new(_info: aviutl2::AviUtl2Info) -> AnyResult<Self> {
        Ok(Self)
    }

    fn plugin_info(&self) -> aviutl2::module::ScriptModuleTable {
        aviutl2::module::ScriptModuleTable {
            information: format!(
                "svg_trace.mod2 bitmap→SVG / v{} / HexBrowns",
                env!("CARGO_PKG_VERSION")
            ),
            functions: Self::functions(),
        }
    }
}

#[link(name = "kernel32")]
unsafe extern "system" {
    safe fn GetCurrentThreadId() -> u32;
}

/// 貸し出しのキー。Rust の `thread::current()` はホストのスレッドに TLS のデストラクタを
/// 登録するので使わない（DLL が外れた後に走らせたくない）
fn current_thread_id() -> u32 {
    GetCurrentThreadId()
}

fn config_from_param(params: &ScriptModuleCallHandle, index: usize) -> TraceConfig {
    // aviutl2 0.47: 省略・nil は Ok(None)、テーブル以外は Err。どちらも既定値にする
    match params.get_param::<Option<ScriptModuleParamTable<'_>>>(index) {
        Ok(Some(t)) => TraceConfig::from_table(&t),
        _ => TraceConfig::default(),
    }
}

/// 引数の文字列。省略・nil・空文字列は None
fn opt_str(params: &ScriptModuleCallHandle, index: usize) -> Option<String> {
    params
        .get_param::<Option<String>>(index)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
}

/// 自動書き出しが ON のときだけ書き出し先を決める（aux2 のサイドカーを読むので、Mutex の外で）
fn export_dir_for(cfg: &TraceConfig) -> Option<PathBuf> {
    cfg.auto_export.then(trace::export_dir)
}

/// トレース結果（失敗も）をスロットに入れて、Lua へ返す。
///
/// 失敗もスロットに覚えるので、同じ入力のうちは `render_cached` が失敗を返し、毎フレームトレースし直さない
fn finish_vectorize(
    params: &mut ScriptModuleCallHandle,
    result: anyhow::Result<TraceResult>,
    cfg: &TraceConfig,
    source: Option<SourceStamp>,
) {
    let now = Instant::now();
    let export_dir = export_dir_for(cfg);
    match result {
        Ok(r) => {
            let (w, h) = (r.width as i32, r.height as i32);
            let svg = r.svg.clone();
            STORE.store_result(Ok(r), cfg, source, export_dir.as_deref(), now);
            let _ = params.push_result((svg, w, h));
        }
        Err(e) => {
            let msg = format!("{e:#}");
            STORE.store_result(Err(msg.clone()), cfg, source, None, now);
            let _ = params.set_error(&msg);
        }
    }
}

/// 失敗を覚えずに Lua へ返す（ファイルが見つからないとき。置かれたらすぐ読めるように、毎回確かめる）
fn fail_without_cache(params: &mut ScriptModuleCallHandle, e: anyhow::Error) {
    let _ = params.set_error(&format!("{e:#}"));
}

#[aviutl2::module::functions]
impl SvgTraceModule {
    /// vectorize_file(path, config_table?) -> svg, width, height
    ///
    /// config_table の `instance` / `cache_key` でスロットに入れ、`auto_export` が 1 のときだけ書き出す。
    /// 読んだ画像は持っておき、スライダーを動かしてトレースし直すときに読み直さない
    #[direct]
    fn vectorize_file(&self, params: &mut ScriptModuleCallHandle) {
        let Some(path) = opt_str(params, 0) else {
            let _ = params.set_error("vectorize_file: path required");
            return;
        };
        let cfg = config_from_param(params, 1);
        let (result, source) = match STORE.decoded_image(Path::new(&path), Instant::now()) {
            Ok((img, stamp)) => (vectorize_image(&img, &cfg), stamp),
            Err(e) => match SourceStamp::of(Path::new(&path)) {
                // ファイルはあるが読めない（壊れている・大きすぎる）: 書き換わるまで失敗を覚える
                Some(stamp) => (Err(e), Some(stamp)),
                None => return fail_without_cache(params, e),
            },
        };
        finish_vectorize(params, result, &cfg, source);
    }

    /// vectorize_rgba(ptr, w, h, config_table?) -> svg, width, height
    #[direct]
    fn vectorize_rgba(&self, params: &mut ScriptModuleCallHandle) {
        let Some((pixels, w, h)) = pixels_param(params) else {
            return;
        };
        let cfg = config_from_param(params, 3);
        let result = vectorize_rgba(pixels, w, h, &cfg);
        finish_vectorize(params, result, &cfg, None);
    }

    /// content_key(ptr, w, h) -> string
    ///
    /// レイヤー入力の画素の中身を表すキー（大きさと xxh3）。obj2 v0.2.0 はこれをキャッシュのキーに入れる
    /// （v0.1.3 まではフレーム番号を入れていて、下が静止画でも毎フレームトレースし直していた）
    #[direct]
    fn content_key(&self, params: &mut ScriptModuleCallHandle) {
        let Some((pixels, w, h)) = pixels_param(params) else {
            return;
        };
        let _ = params.push_result(content_key(pixels, w, h));
    }

    /// render_cached(instance, key) -> data_ptr, width, height
    ///
    /// そのインスタンスのスロットが同じ入力のキーで作られていれば、描いた画素を返す。
    /// 無ければ何も返さない（Lua 側はトレースする。v0.1.4 まではエラーにしていた）。
    /// そのキーでトレースに失敗していたら、その理由をエラーで返す（Lua 側はトレースし直さない）。
    /// ファイル入力は元画像の大きさか更新日時が変わっていたら当てない
    #[direct]
    fn render_cached(&self, params: &mut ScriptModuleCallHandle) {
        let instance = opt_str(params, 0).unwrap_or_default();
        let key = opt_str(params, 1).unwrap_or_default();
        let now = Instant::now();
        match STORE.render_cached(&instance, &key, now) {
            Cached::Hit(raster, w, h) => {
                // 画素はスロットが捨てられても貸し出しが持つ。ポインタを Lua へ返した後に Mutex が外れても、
                // このスレッドが次に受け取るまでは解放されない（store.rs の「Lua へ返すポインタの寿命」）
                let ptr = STORE.lend(current_thread_id(), raster, now);
                let _ = params.push_result((ptr, w as i32, h as i32));
            }
            Cached::Miss => {}
            Cached::Failed(msg) => {
                let _ = params.set_error(&msg);
            }
        }
    }

    /// rasterize_svg(svg?) -> data_ptr, width, height
    ///
    /// 互換用。svg 省略時は直前のトレース結果（どのインスタンスかは問わない）を描く。
    /// SVG_TRACE.obj2 v0.1.3 からは render_cached を使う
    #[direct]
    fn rasterize_svg(&self, params: &mut ScriptModuleCallHandle) {
        let svg: Arc<str> = match opt_str(params, 0) {
            Some(s) => Arc::from(s),
            None => STORE
                .with(|s| s.svg_for(None))
                .map(|(svg, _, _)| svg)
                .unwrap_or_else(|| Arc::from("")),
        };
        if svg.is_empty() {
            let _ = params.set_error("rasterize_svg: no svg");
            return;
        }
        match rasterize_svg(&svg) {
            Ok((buf, w, h)) => {
                // render_cached と同じく、このスレッドの貸し出しに持たせる
                let ptr = STORE.lend(current_thread_id(), Arc::new(buf), Instant::now());
                let _ = params.push_result((ptr, w as i32, h as i32));
            }
            Err(e) => {
                let _ = params.set_error(&format!("{e:#}"));
            }
        }
    }

    /// get_last_svg() -> svg, width, height
    fn get_last_svg(&self) -> AnyResult<(String, i32, i32)> {
        let (svg, w, h) = STORE
            .with(|s| s.svg_for(None))
            .map(|(svg, w, h)| (svg.to_string(), w, h))
            .unwrap_or_default();
        Ok((svg, w as i32, h as i32))
    }

    /// write_latest(path?, instance?) -> path
    ///
    /// instance を渡せばそのインスタンスの結果、無ければ直前のトレース結果を書く。
    /// path 省略時は現在の export_dir()/latest.svg。前に同じ内容を書いていれば書かない
    #[direct]
    fn write_latest(&self, params: &mut ScriptModuleCallHandle) {
        let instance = opt_str(params, 1);
        let path = opt_str(params, 0)
            .map(PathBuf::from)
            .unwrap_or_else(trace::latest_svg_path);
        match STORE.write_svg(&path, instance.as_deref()) {
            Ok(_) => {
                let _ = params.push_result(path.to_string_lossy().into_owned());
            }
            Err(e) => {
                let _ = params.set_error(&format!("{e:#}"));
            }
        }
    }

    /// latest_path() -> path
    fn latest_path(&self) -> String {
        trace::latest_svg_path().to_string_lossy().into_owned()
    }
}

/// 引数の (ptr, w, h) を画素の slice にする。無効なら Lua へエラーを返して None
fn pixels_param<'a>(params: &mut ScriptModuleCallHandle) -> Option<(&'a [u8], u32, u32)> {
    // aviutl2 0.47: get_param_data は Result<*mut T>。null も確かめる
    let ptr = match params.get_param_data::<u8>(0) {
        Ok(p) if !p.is_null() => p,
        _ => {
            let _ = params.set_error("pixel pointer required");
            return None;
        }
    };
    let w = params.get_param_int(1).unwrap_or(0).max(0) as u32;
    let h = params.get_param_int(2).unwrap_or(0).max(0) as u32;
    let len = (w as usize).saturating_mul(h as usize).saturating_mul(4);
    if len == 0 {
        let _ = params.set_error("empty size");
        return None;
    }
    // obj.getpixeldata が返した画素。同じ Lua の呼び出しの間は有効
    Some((unsafe { std::slice::from_raw_parts(ptr as *const u8, len) }, w, h))
}

/// aux2（`svg_trace.aux2`）が、同じプロセスに読み込まれたこのモジュールから SVG を読む入口。
///
/// v0.1.4 までは、そのためにトレースのたびに `Plugin/svg_trace/latest_trace.svg` と `traces/{obj.id}.svg` を
/// 書いていた（レイヤー入力で絵が動くと毎フレーム 2 本）。今はメモリの結果を写すだけで、ファイルを書かない。
///
/// - `selector` 0: `object_id`（SDK の `get_object_id`）のオブジェクトの結果 / 1: 直前のトレース結果
/// - 戻り値: 無ければ -1。あれば SVG のバイト数（UTF-8）。`cap` がそれ以上なら `buf` に写す
///   （aux2 はまず `cap` 0 で長さを聞き、確保してからもう一度呼ぶ）
///
/// # Safety
///
/// `buf` は null か、`cap` バイト書ける領域
#[unsafe(no_mangle)]
pub unsafe extern "C" fn svg_trace_copy_svg_v1(selector: i32, object_id: i64, buf: *mut u8, cap: usize) -> i64 {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let svg = STORE.with(|s| match selector {
            0 => s.svg_for_object(object_id),
            1 => s.svg_for(None).map(|(svg, _, _)| svg),
            _ => None,
        });
        let Some(svg) = svg else {
            return -1;
        };
        let bytes = svg.as_bytes();
        if !buf.is_null() && cap >= bytes.len() {
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, bytes.len()) };
        }
        bytes.len() as i64
    }))
    .unwrap_or(-1)
}

aviutl2::register_script_module!(SvgTraceModule);

#[cfg(test)]
mod tests {
    use super::*;

    /// aux2 が使う入口: 長さを聞いてから写す。無ければ -1
    #[test]
    fn copy_svg_for_aux2() {
        let cfg = TraceConfig {
            instance: "4242".into(),
            cache_key: "k".into(),
            ..TraceConfig::default()
        };
        let r = TraceResult {
            svg: "<svg id=\"copy_svg_for_aux2\"/>".into(),
            width: 1,
            height: 1,
        };
        STORE.store_result(Ok(r), &cfg, None, None, Instant::now());
        let len = unsafe { svg_trace_copy_svg_v1(0, 4242, std::ptr::null_mut(), 0) };
        assert!(len > 0);
        let mut buf = vec![0u8; len as usize];
        let len2 = unsafe { svg_trace_copy_svg_v1(0, 4242, buf.as_mut_ptr(), buf.len()) };
        assert_eq!(len, len2);
        assert_eq!(String::from_utf8(buf).unwrap(), "<svg id=\"copy_svg_for_aux2\"/>");
        // 小さすぎる領域には写さない
        let mut small = vec![0u8; 4];
        assert_eq!(unsafe { svg_trace_copy_svg_v1(0, 4242, small.as_mut_ptr(), 4) }, len);
        assert_eq!(small, [0, 0, 0, 0]);
        assert_eq!(unsafe { svg_trace_copy_svg_v1(0, 999_999, std::ptr::null_mut(), 0) }, -1);
        assert_eq!(unsafe { svg_trace_copy_svg_v1(7, 4242, std::ptr::null_mut(), 0) }, -1);
    }
}
