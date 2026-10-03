mod store;
mod trace;

#[cfg(test)]
mod smoke_test;

use aviutl2::{
    module::{ScriptModuleCallHandle, ScriptModuleFunctions, ScriptModuleParamTable},
    AnyResult,
};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use store::{ExportTargets, SourceStamp, TraceStore};
use trace::{rasterize_svg, vectorize_file, vectorize_rgba, write_latest_svg, TraceConfig, TraceResult};

#[aviutl2::plugin(ScriptModule)]
struct SvgTraceModule {
    store: Mutex<TraceStore>,
    /// 引数付きの `rasterize_svg(svg)` の描画先（互換用）
    preview: Mutex<Vec<u8>>,
}

impl aviutl2::module::ScriptModule for SvgTraceModule {
    fn new(_info: aviutl2::AviUtl2Info) -> AnyResult<Self> {
        Ok(Self {
            store: Mutex::new(TraceStore::new()),
            preview: Mutex::new(Vec::new()),
        })
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

impl SvgTraceModule {
    fn lock_store(&self) -> AnyResult<MutexGuard<'_, TraceStore>> {
        self.store.lock().map_err(|e| anyhow::anyhow!("{e}"))
    }
}

fn export_targets() -> ExportTargets {
    ExportTargets {
        export_dir: trace::export_dir(),
        handoff_path: trace::handoff_svg_path(),
    }
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

fn finish_vectorize(
    module: &SvgTraceModule,
    params: &mut ScriptModuleCallHandle,
    result: anyhow::Result<TraceResult>,
    cfg: &TraceConfig,
    source: Option<SourceStamp>,
) {
    let result = match result {
        Ok(r) => r,
        Err(e) => {
            let _ = params.set_error(&format!("{e:#}"));
            return;
        }
    };
    let stored = module
        .lock_store()
        .and_then(|mut s| s.store_result(&result, cfg, source, &export_targets()));
    if let Err(e) = stored {
        let _ = params.set_error(&format!("{e:#}"));
        return;
    }
    let _ = params.push_result((result.svg, result.width as i32, result.height as i32));
}

#[aviutl2::module::functions]
impl SvgTraceModule {
    /// vectorize_file(path, config_table?) -> svg, width, height
    ///
    /// config_table の `instance` / `cache_key` でスロットに入れ、`auto_export` が 1 のときだけ書き出す
    #[direct]
    fn vectorize_file(&self, params: &mut ScriptModuleCallHandle) {
        let Some(path) = opt_str(params, 0) else {
            let _ = params.set_error("vectorize_file: path required");
            return;
        };
        let cfg = config_from_param(params, 1);
        let source = SourceStamp::of(Path::new(&path));
        let result = vectorize_file(Path::new(&path), &cfg);
        finish_vectorize(self, params, result, &cfg, source);
    }

    /// vectorize_rgba(ptr, w, h, config_table?) -> svg, width, height
    #[direct]
    fn vectorize_rgba(&self, params: &mut ScriptModuleCallHandle) {
        // aviutl2 0.47: get_param_data は Result<*mut T>。null も確かめる
        let ptr = match params.get_param_data::<u8>(0) {
            Ok(p) if !p.is_null() => p,
            _ => {
                let _ = params.set_error("vectorize_rgba: pixel pointer required");
                return;
            }
        };
        let w = params.get_param_int(1).unwrap_or(0).max(0) as u32;
        let h = params.get_param_int(2).unwrap_or(0).max(0) as u32;
        let cfg = config_from_param(params, 3);
        let len = (w as usize).saturating_mul(h as usize).saturating_mul(4);
        if len == 0 {
            let _ = params.set_error("vectorize_rgba: empty size");
            return;
        }
        let pixels = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
        let result = vectorize_rgba(pixels, w, h, &cfg);
        finish_vectorize(self, params, result, &cfg, None);
    }

    /// render_cached(instance, key) -> data_ptr, width, height
    ///
    /// そのインスタンスのスロットが同じ入力のキーで作られていれば、描いた画素を返す。
    /// 無ければエラー（Lua 側は pcall で受けてトレースし直す）。
    /// ファイル入力は元画像の大きさか更新日時が変わっていたら当てない
    #[direct]
    fn render_cached(&self, params: &mut ScriptModuleCallHandle) {
        let instance = opt_str(params, 0).unwrap_or_default();
        let key = opt_str(params, 1).unwrap_or_default();
        let hit = self
            .lock_store()
            .and_then(|mut s| s.render_cached(&instance, &key));
        match hit {
            Ok(Some((ptr, w, h))) => {
                let _ = params.push_result((ptr, w as i32, h as i32));
            }
            Ok(None) => {
                let _ = params.set_error("render_cached: miss");
            }
            Err(e) => {
                let _ = params.set_error(&format!("{e:#}"));
            }
        }
    }

    /// rasterize_svg(svg?) -> data_ptr, width, height
    ///
    /// 互換用。svg 省略時は直前のトレース結果（どのインスタンスかは問わない）を描く。
    /// SVG_TRACE.obj2 v0.1.3 からは render_cached を使う
    #[direct]
    fn rasterize_svg(&self, params: &mut ScriptModuleCallHandle) {
        let svg = match opt_str(params, 0) {
            Some(s) => s,
            None => match self.lock_store() {
                Ok(s) => s.last().map(|r| r.svg.clone()).unwrap_or_default(),
                Err(e) => {
                    let _ = params.set_error(&format!("{e:#}"));
                    return;
                }
            },
        };
        if svg.is_empty() {
            let _ = params.set_error("rasterize_svg: no svg");
            return;
        }
        match rasterize_svg(&svg) {
            Ok((buf, w, h)) => {
                let mut preview = match self.preview.lock() {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = params.set_error(&format!("{e}"));
                        return;
                    }
                };
                *preview = buf;
                let ptr = preview.as_ptr();
                let _ = params.push_result((ptr, w as i32, h as i32));
            }
            Err(e) => {
                let _ = params.set_error(&format!("{e:#}"));
            }
        }
    }

    /// get_last_svg() -> svg, width, height
    fn get_last_svg(&self) -> AnyResult<(String, i32, i32)> {
        let store = self.lock_store()?;
        let (svg, w, h) = store.svg_for(None).unwrap_or_default();
        Ok((svg, w as i32, h as i32))
    }

    /// write_latest(path?, instance?) -> path
    ///
    /// instance を渡せばそのインスタンスの結果、無ければ直前のトレース結果を書く。
    /// path 省略時は現在の export_dir()/latest.svg
    #[direct]
    fn write_latest(&self, params: &mut ScriptModuleCallHandle) {
        let instance = opt_str(params, 1);
        let svg = match self.lock_store() {
            Ok(s) => s
                .svg_for(instance.as_deref())
                .map(|(svg, _, _)| svg)
                .unwrap_or_default(),
            Err(e) => {
                let _ = params.set_error(&format!("{e:#}"));
                return;
            }
        };
        if svg.is_empty() {
            let _ = params.set_error("write_latest: no svg");
            return;
        }
        let path = match opt_str(params, 0) {
            Some(p) => {
                if let Some(parent) = Path::new(&p).parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::write(&p, svg.as_bytes()) {
                    Ok(()) => p,
                    Err(e) => {
                        let _ = params.set_error(&format!("{e}"));
                        return;
                    }
                }
            }
            None => match write_latest_svg(&svg) {
                Ok(p) => p.to_string_lossy().into_owned(),
                Err(e) => {
                    let _ = params.set_error(&format!("{e:#}"));
                    return;
                }
            },
        };
        let _ = params.push_result(path);
    }

    /// latest_path() -> path
    fn latest_path(&self) -> String {
        trace::latest_svg_path().to_string_lossy().into_owned()
    }
}

aviutl2::register_script_module!(SvgTraceModule);
