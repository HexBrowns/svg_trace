use aviutl2::{
    anyhow::Context,
    tracing,
};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

static EDIT_HANDLE: aviutl2::generic::GlobalEditHandle = aviutl2::generic::GlobalEditHandle::new();

/// このプラグインを読み込んだ時刻。インスタンスごとの受け渡しファイルは、これより後に
/// 書かれたものだけを使う（オブジェクト ID はアプリ起動ごとに振り直されるので、前の起動で
/// 書かれたファイルは別のオブジェクトのものでありうる）
static SESSION_START: OnceLock<SystemTime> = OnceLock::new();

#[aviutl2::plugin(GenericPlugin)]
struct SvgTraceAux2;

impl aviutl2::generic::GenericPlugin for SvgTraceAux2 {
    fn new(_info: aviutl2::AviUtl2Info) -> aviutl2::AnyResult<Self> {
        aviutl2::tracing_subscriber::fmt()
            .with_max_level(if cfg!(debug_assertions) {
                tracing::Level::DEBUG
            } else {
                tracing::Level::INFO
            })
            .event_format(aviutl2::logger::AviUtl2Formatter)
            .with_writer(aviutl2::logger::AviUtl2LogWriter)
            .init();
        let _ = SESSION_START.set(SystemTime::now());
        cleanup_old_handoff_files();
        let _ = write_export_root_sidecar(&fallback_export_dir());
        Ok(Self)
    }

    fn plugin_info(&self) -> aviutl2::generic::GenericPluginTable {
        aviutl2::generic::GenericPluginTable {
            name: "svg_trace.aux2".to_string(),
            information: format!(
                "SVG_TRACE helper (SVG_H handoff) / v{} / HexBrowns",
                env!("CARGO_PKG_VERSION")
            ),
        }
    }

    fn register(&mut self, registry: &mut aviutl2::generic::HostAppHandle) {
        EDIT_HANDLE.init(registry.create_edit_handle());
        registry.register_object_menu("SVG_TRACE\\SVG_Hへ送る", || {
            report_menu_result("SVG_Hへ送る", send_to_svg_h());
        });
        registry.register_export_menu("SVG_TRACE\\最新SVGを確認書き出し", || {
            report_menu_result("最新SVGを確認書き出し", confirm_latest_export());
        });
    }

    fn on_project_load(&mut self, project: &mut aviutl2::generic::ProjectFile) {
        update_export_root_from_project(project.get_path().as_deref());
    }

    fn on_project_save(&mut self, project: &mut aviutl2::generic::ProjectFile) {
        update_export_root_from_project(project.get_path().as_deref());
    }
}

fn fallback_export_dir() -> PathBuf {
    aviutl2::config::app_data_path()
        .join("Plugin")
        .join("svg_trace")
        .join("export")
}

fn export_root_sidecar_path() -> PathBuf {
    aviutl2::config::app_data_path()
        .join("Plugin")
        .join("svg_trace")
        .join("export_root.txt")
}

/// v0.1.3〜v0.1.4 の mod2 がトレースのたびに上書きしていた受け渡しファイル。
/// v0.2.0 の mod2 は書かない（`read_svg_from_module` でメモリから読む）。古い mod2 と組んだときだけ読む
fn handoff_svg_path() -> PathBuf {
    aviutl2::config::app_data_path()
        .join("Plugin")
        .join("svg_trace")
        .join("latest_trace.svg")
}

/// v0.1.4 の mod2 がインスタンス（オブジェクト ID）ごとに書いていた受け渡しファイルの置き場。
/// v0.2.0 の mod2 は書かない。起動のたびに増えていたので、起動時に片付ける（`cleanup_old_handoff_files`）
fn instance_svg_dir() -> PathBuf {
    aviutl2::config::app_data_path()
        .join("Plugin")
        .join("svg_trace")
        .join("traces")
}

/// オブジェクト ID の受け渡しファイル。この起動の間に書かれたものだけを返す
fn instance_svg_source(object_id: i64, session_start: SystemTime) -> Option<PathBuf> {
    instance_svg_source_in(&instance_svg_dir(), object_id, session_start)
}

fn instance_svg_source_in(dir: &Path, object_id: i64, session_start: SystemTime) -> Option<PathBuf> {
    if object_id == 0 {
        // SDK の get_object_id は取得できないとき 0 を返す
        return None;
    }
    let path = dir.join(format!("{object_id}.svg"));
    let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
    (modified >= session_start).then_some(path)
}

/// 古い mod2 が書いた、直前にトレースした SVG。この起動の間に書かれたものだけを返す
fn latest_svg_source(session_start: SystemTime) -> Option<PathBuf> {
    let path = handoff_svg_path();
    let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
    (modified >= session_start).then_some(path)
}

/// 前の起動で古い mod2 が書いた受け渡しファイルを片付ける（オブジェクト ID は起動ごとに振り直されるので、
/// 前の起動のファイルは使えない。v0.1.4 では起動のたびに `traces/` に増えていた）
fn cleanup_old_handoff_files() {
    let removed = cleanup_handoff_files_in(&instance_svg_dir(), &handoff_svg_path());
    if removed > 0 {
        tracing::info!("svg_trace: 前の起動の受け渡しファイルを {removed} 個片付けた");
    }
}

/// `traces/` の `{ID}.svg` と `latest_trace.svg` を消す。消した数を返す。`traces/` は空になったら消す
fn cleanup_handoff_files_in(dir: &Path, handoff: &Path) -> usize {
    let mut removed = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".svg")) else {
                continue;
            };
            // mod2 が書いた名前（オブジェクト ID か、英数字と `_` `-`）だけ
            let ours = !stem.is_empty()
                && stem.len() <= 64
                && stem.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            if ours && entry.path().is_file() && std::fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
        let _ = std::fs::remove_dir(dir); // 空でなければ失敗する（ほかのファイルは残す）
    }
    if handoff.is_file() && std::fs::remove_file(handoff).is_ok() {
        removed += 1;
    }
    removed
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleExW(flags: u32, module_name: *const u16, module: *mut *mut c_void) -> i32;
    fn GetProcAddress(module: *mut c_void, proc_name: *const u8) -> *mut c_void;
    fn FreeLibrary(module: *mut c_void) -> i32;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn MessageBoxW(hwnd: *mut c_void, text: *const u16, caption: *const u16, kind: u32) -> i32;
}

/// mod2 の `svg_trace_copy_svg_v1`（selector, object_id, buf, cap）-> 長さ（無ければ -1）
type CopySvgFn = unsafe extern "C" fn(i32, i64, *mut u8, usize) -> i64;

/// 本体が読み込んだスクリプトモジュールのファイル名
const MOD2_NAME: &str = "svg_trace.mod2";

/// mod2 から読んだ SVG
#[derive(Debug, PartialEq, Eq)]
enum ModuleSvg {
    Found(String),
    /// mod2 はあるが、その結果が無い
    NotFound,
    /// mod2 が読み込まれていない・古い（理由）
    Unavailable(&'static str),
}

/// 同じプロセスに読み込まれた mod2 から、トレース結果をメモリのまま読む。
///
/// v0.1.4 までは mod2 がトレースのたびにファイル（`latest_trace.svg` と `traces/{ID}.svg`）を書き、
/// ここでそれを読んでいた。selector 0 は `object_id` のオブジェクトの結果、1 は直前のトレース結果
fn read_svg_from_module(module_name: &str, selector: i32, object_id: i64) -> ModuleSvg {
    let wide: Vec<u16> = module_name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut module: *mut c_void = std::ptr::null_mut();
    // 参照を 1 つ増やして取る（読んでいる間に外れないように）。最後に FreeLibrary で戻す
    if unsafe { GetModuleHandleExW(0, wide.as_ptr(), &mut module) } == 0 || module.is_null() {
        return ModuleSvg::Unavailable("svg_trace.mod2 が読み込まれていない");
    }
    struct Release(*mut c_void);
    impl Drop for Release {
        fn drop(&mut self) {
            unsafe { FreeLibrary(self.0) };
        }
    }
    let _release = Release(module);
    let proc = unsafe { GetProcAddress(module, c"svg_trace_copy_svg_v1".as_ptr().cast()) };
    if proc.is_null() {
        return ModuleSvg::Unavailable("svg_trace.mod2 が v0.1.4 以前");
    }
    let copy: CopySvgFn = unsafe { std::mem::transmute::<*mut c_void, CopySvgFn>(proc) };
    // 長さを聞いてから写す。間にトレースし直されて長さが変われば聞き直す
    for _ in 0..4 {
        let len = unsafe { copy(selector, object_id, std::ptr::null_mut(), 0) };
        if len < 0 {
            return ModuleSvg::NotFound;
        }
        let mut buf = vec![0u8; len as usize];
        let copied = unsafe { copy(selector, object_id, buf.as_mut_ptr(), buf.len()) };
        if copied == len {
            return ModuleSvg::Found(String::from_utf8_lossy(&buf).into_owned());
        }
        if copied < 0 {
            return ModuleSvg::NotFound;
        }
    }
    ModuleSvg::NotFound
}

/// メニューの結果を出す。断ったとき・失敗したときは、ログに加えてメッセージを出す
/// （メニューを選んでも何も起きないと、理由が分からない）
fn report_menu_result(menu: &str, result: aviutl2::AnyResult<Result<String, String>>) {
    let text = match result {
        Ok(Ok(done)) => {
            tracing::info!("{menu}: {done}");
            return;
        }
        Ok(Err(reason)) => {
            tracing::warn!("{menu}: {reason}");
            reason
        }
        Err(e) => {
            tracing::error!("{menu} failed: {e:#}");
            format!("失敗しました: {e:#}")
        }
    };
    show_message(&text);
}

fn show_message(text: &str) {
    const MB_ICONWARNING: u32 = 0x30;
    let owner = if EDIT_HANDLE.is_ready() {
        EDIT_HANDLE
            .get_host_app_window_raw()
            .map_or(std::ptr::null_mut(), |h| h.hwnd.get() as *mut c_void)
    } else {
        std::ptr::null_mut()
    };
    let text: Vec<u16> = wrap_for_message(text, MESSAGE_LINE_CHARS).encode_utf16().chain(std::iter::once(0)).collect();
    let caption: Vec<u16> = "SVG_TRACE".encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { MessageBoxW(owner, text.as_ptr(), caption.as_ptr(), MB_ICONWARNING) };
}

/// メッセージボックスの 1 行の字数の目安
const MESSAGE_LINE_CHARS: usize = 30;

/// メッセージボックスは空白の無い日本語の並びを折り返さず、右端で切る（実機）。
/// 「。」の後で改行し、長い行は目安の字数を超えたところの「、」「）」・空白の後で折り返す（無ければ目安の 1.5 倍で切る）
fn wrap_for_message(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut len = 0;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        len += 1;
        let end = chars.peek().is_none();
        // 閉じ括弧・句読点は行の頭に置かない（「。」だけが次の行に落ちる）
        let next_closes = matches!(chars.peek(), Some('。' | '、' | '）' | ')' | '」'));
        let sentence = c == '。' || c == '\n';
        let soft = len >= width && matches!(c, '、' | '）' | ')' | ' ' | '　');
        if c == '\n' {
            len = 0;
        } else if !end && !next_closes && (sentence || soft || len >= width * 3 / 2) {
            out.push('\n');
            len = 0;
        }
    }
    out
}

#[cfg(test)]
mod wrap_tests {
    use super::wrap_for_message;

    #[test]
    fn long_japanese_is_wrapped() {
        let s = "レイヤー9 の開始フレーム 1005（0 始まり）のオブジェクト（ID 19） のトレース結果がありません（SVG_TRACE でないか、AviUtl2 を起動してからまだ描いていない）。SVG_TRACE のオブジェクトを、プレビューに表示してから送ってください";
        let w = wrap_for_message(s, 30);
        assert!(w.lines().all(|l| l.chars().count() <= 45), "{w}");
        assert_eq!(w.replace('\n', ""), s, "折り返し以外の文字は変えない");
        assert!(!w.ends_with('\n'));
        assert!(w.lines().all(|l| !l.starts_with(['。', '、', '）'])), "句読点・閉じ括弧で行を始めない: {w}");
    }
}

fn export_dir_from_project(project_path: Option<&Path>) -> PathBuf {
    if let Some(p) = project_path {
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                return parent.join("SVG_export");
            }
        }
    }
    fallback_export_dir()
}

/// 書き出し先を受け渡しファイルに書く。中身が同じなら書かずログも出さない
/// （プロジェクトの保存のたびに呼ばれ、本体の自動バックアップで 1 分ごとに来る）
fn write_export_root_sidecar(dir: &Path) -> aviutl2::AnyResult<()> {
    let sidecar = export_root_sidecar_path();
    let text = dir.to_string_lossy();
    if std::fs::read_to_string(&sidecar).is_ok_and(|old| old == text) {
        return Ok(());
    }
    if let Some(parent) = sidecar.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&sidecar, text.as_bytes())
        .with_context(|| format!("Failed to write {}", sidecar.display()))?;
    tracing::info!("svg_trace export root → {}", dir.display());
    Ok(())
}

fn update_export_root_from_project(project_path: Option<&Path>) {
    let dir = export_dir_from_project(project_path);
    if let Err(e) = write_export_root_sidecar(&dir) {
        tracing::warn!("export_root sidecar update failed: {e:#}");
    }
}

fn resolve_export_dir_in_edit(edit: &aviutl2::generic::EditSection) -> PathBuf {
    let path = edit.get_project_file(&EDIT_HANDLE).get_path();
    let dir = export_dir_from_project(path.as_deref());
    let _ = write_export_root_sidecar(&dir);
    dir
}

/// 「最新SVGを確認書き出し」: 直前にトレースした結果を書き出し先へ複製する。
/// 戻り値の `Err` は利用者に見せる断りの理由
fn confirm_latest_export() -> aviutl2::AnyResult<Result<String, String>> {
    let session_start = SESSION_START.get().copied().unwrap_or(SystemTime::UNIX_EPOCH);
    EDIT_HANDLE.call_edit_section(|edit| -> aviutl2::AnyResult<Result<String, String>> {
        let export_dir = resolve_export_dir_in_edit(edit);
        let svg = match read_svg_from_module(MOD2_NAME, 1, 0) {
            ModuleSvg::Found(svg) => svg,
            ModuleSvg::NotFound => {
                return Ok(Err("書き出す SVG がありません。先に SVG_TRACE でトレースしてください".into()));
            }
            ModuleSvg::Unavailable(why) => match latest_svg_source(session_start) {
                Some(src) => std::fs::read_to_string(&src).with_context(|| format!("read {}", src.display()))?,
                None => {
                    return Ok(Err(format!(
                        "書き出す SVG がありません（{why}）。先に SVG_TRACE でトレースしてください"
                    )));
                }
            },
        };
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        std::fs::create_dir_all(&export_dir)?;
        let dst = export_dir.join(format!("trace_{stamp}.svg"));
        std::fs::write(&dst, svg.as_bytes()).with_context(|| format!("write {}", dst.display()))?;
        Ok(Ok(format!("Exported {}", dst.display())))
    })?
}

/// 「SVG_Hへ送る」の対象。右クリックしたオブジェクトを取る。
///
/// オブジェクトメニューのコールバックには対象のオブジェクトが渡されないので、
/// 1. マウスの位置（右クリックした所）にあるオブジェクトが選択の中にあれば、それ
/// 2. 無ければ、オブジェクト設定ウィンドウのフォーカス
/// 3. それも無く、選択が 1 つだけなら、それ
fn menu_target_object(
    edit: &aviutl2::generic::EditSection,
) -> Option<aviutl2::generic::ObjectHandle> {
    let selected = edit.get_selected_objects().unwrap_or_default();
    if let Ok(Some(pos)) = edit.get_mouse_layer_frame() {
        if let Ok(Some(h)) = edit.find_object_after(pos.layer, pos.frame) {
            let under_mouse = edit
                .get_object_layer_frame(h)
                .map(|lf| lf.layer == pos.layer && lf.start <= pos.frame && pos.frame <= lf.end)
                .unwrap_or(false);
            if under_mouse && selected.contains(&h) {
                return Some(h);
            }
        }
    }
    if let Ok(Some(h)) = edit.get_focused_object() {
        if edit.object_exists(h) {
            return Some(h);
        }
    }
    match selected.as_slice() {
        [h] if edit.object_exists(*h) => Some(*h),
        _ => None,
    }
}

/// ログに出すオブジェクトの説明（レイヤーと開始フレーム、ID）
fn describe_object(
    edit: &aviutl2::generic::EditSection,
    object: aviutl2::generic::ObjectHandle,
    object_id: i64,
) -> String {
    match edit.get_object_layer_frame(object) {
        Ok(lf) => format!(
            "レイヤー{} の開始フレーム {}（0 始まり）のオブジェクト（ID {object_id}）",
            lf.layer + 1,
            lf.start
        ),
        Err(_) => format!("オブジェクト（ID {object_id}）"),
    }
}

/// 「SVG_Hへ送る」: 右クリックしたオブジェクトのトレース結果で SVG_H のオブジェクトを作る。
///
/// そのオブジェクトの結果が無ければ送らずに理由を出す（v0.1.4 までは、全体で最後にトレースした
/// 別のオブジェクトの結果を送っていた）。戻り値の `Err` は利用者に見せる断りの理由
fn send_to_svg_h() -> aviutl2::AnyResult<Result<String, String>> {
    let session_start = SESSION_START.get().copied().unwrap_or(SystemTime::UNIX_EPOCH);
    EDIT_HANDLE.call_edit_section(|edit| -> aviutl2::AnyResult<Result<String, String>> {
        let export_dir = resolve_export_dir_in_edit(edit);

        let Some(target) = menu_target_object(edit) else {
            return Ok(Err(
                "送るオブジェクトが分かりません。タイムラインの SVG_TRACE のオブジェクトを右クリックして選んでから使ってください"
                    .into(),
            ));
        };
        let object_id = edit.get_object_id(target).unwrap_or(0);
        let what = describe_object(edit, target, object_id);
        let missing = |why: &str| {
            format!(
                "{what} のトレース結果がありません（{why}）。SVG_TRACE のオブジェクトを、プレビューに表示してから送ってください"
            )
        };
        let (svg, src) = match read_svg_from_module(MOD2_NAME, 0, object_id) {
            ModuleSvg::Found(svg) => (svg, "svg_trace.mod2".to_string()),
            ModuleSvg::NotFound => {
                return Ok(Err(missing(
                    "SVG_TRACE でないか、AviUtl2 を起動してからまだ表示していないか、トレースに失敗している",
                )));
            }
            // 古い mod2 と組んだとき: その mod2 が書いたファイルを読む
            ModuleSvg::Unavailable(why) => match instance_svg_source(object_id, session_start) {
                Some(p) => (
                    std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?,
                    p.display().to_string(),
                ),
                None => return Ok(Err(missing(why))),
            },
        };

        let (svg_w, svg_h) = parse_svg_size(&svg).unwrap_or((400, 400));

        std::fs::create_dir_all(&export_dir)?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let svg_path = export_dir.join(format!("handoff_{stamp}.svg"));
        std::fs::write(&svg_path, svg.as_bytes())?;

        let position = edit
            .get_mouse_layer_frame()?
            .unwrap_or(aviutl2::generic::LayerFrameData {
                layer: edit.info.layer,
                frame: edit.info.frame,
            });

        let mut object_alias = aviutl2::alias::Table::new();
        let mut object = aviutl2::alias::Table::new();
        object.insert_value("name", format!("SVG_H ({stamp})"));
        let mut object_0 = aviutl2::alias::Table::new();
        object_0.insert_value("effect.name", "SVG_H");
        object_0.insert_value(
            "ファイル",
            svg_path
                .to_str()
                .context("SVG path is not valid UTF-8")?,
        );
        object_0.insert_value("幅", svg_w);
        object_0.insert_value("高さ", svg_h);
        object_0.insert_value("塗りを上書き", "0");
        object_0.insert_value("ストロークを上書き", "0");
        object.insert_table("0", object_0);
        object_alias.insert_table("Object", object);

        edit.create_object_from_alias(
            &object_alias.to_string(),
            position.layer,
            position.frame,
            0,
        )?;
        Ok(Ok(format!("{what} のトレース結果を送った（{src} → {}）", svg_path.display())))
    })?
}

fn parse_svg_size(svg: &str) -> Option<(u32, u32)> {
    let w = capture_attr(svg, "width")?;
    let h = capture_attr(svg, "height")?;
    Some((w.max(1), h.max(1)))
}

fn capture_attr(svg: &str, name: &str) -> Option<u32> {
    let key = format!("{name}=\"");
    let i = svg.find(&key)?;
    let rest = &svg[i + key.len()..];
    let end = rest.find('"')?;
    let raw = rest[..end].trim();
    let num: String = raw
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    num.parse::<f32>().ok().map(|v| v.ceil() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_dir(name: &str) -> PathBuf {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("aux2_test")
            .join(name);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// この起動の間に書かれた、その ID のファイルだけを使う
    #[test]
    fn instance_source_requires_this_session() {
        let dir = temp_dir("instance");
        let path = dir.join("42.svg");
        std::fs::write(&path, "<svg/>").unwrap();
        let written = std::fs::metadata(&path).unwrap().modified().unwrap();

        let before = written - Duration::from_secs(10);
        assert_eq!(instance_svg_source_in(&dir, 42, before), Some(path.clone()));
        // 前の起動で書かれたファイル（起動より古い）は使わない
        let after = written + Duration::from_secs(10);
        assert_eq!(instance_svg_source_in(&dir, 42, after), None);
        // 別の ID・取得できなかった ID（0）は使わない
        assert_eq!(instance_svg_source_in(&dir, 43, before), None);
        assert_eq!(instance_svg_source_in(&dir, 0, before), None);
    }

    #[test]
    fn parse_svg_size_reads_width_height() {
        assert_eq!(
            parse_svg_size(r#"<svg width="120.5" height="80" xmlns="x">"#),
            Some((121, 80))
        );
        assert_eq!(parse_svg_size("<svg>"), None);
    }

    /// 起動時の片付け: mod2 が書いた名前の SVG と latest_trace.svg だけを消す
    #[test]
    fn cleanup_removes_only_handoff_files() {
        let root = temp_dir("cleanup");
        let traces = root.join("traces");
        std::fs::create_dir_all(&traces).unwrap();
        for name in ["842.svg", "843.svg", "-7.svg"] {
            std::fs::write(traces.join(name), "<svg/>").unwrap();
        }
        let handoff = root.join("latest_trace.svg");
        std::fs::write(&handoff, "<svg/>").unwrap();
        assert_eq!(cleanup_handoff_files_in(&traces, &handoff), 4);
        assert!(!traces.exists(), "空になった traces が残っている");
        assert!(!handoff.exists());

        // 知らないファイルは残し、フォルダも消さない
        std::fs::create_dir_all(&traces).unwrap();
        std::fs::write(traces.join("1.svg"), "<svg/>").unwrap();
        std::fs::write(traces.join("memo.txt"), "keep").unwrap();
        std::fs::write(traces.join("a b.svg"), "keep").unwrap();
        assert_eq!(cleanup_handoff_files_in(&traces, &handoff), 1);
        assert!(traces.join("memo.txt").exists() && traces.join("a b.svg").exists());
        // 何も無くても失敗しない
        assert_eq!(cleanup_handoff_files_in(&root.join("none"), &root.join("none.svg")), 0);
    }

    /// mod2 が読み込まれていなければ、そう分かる
    #[test]
    fn module_not_loaded_is_reported() {
        assert_eq!(
            read_svg_from_module("svg_trace_not_loaded.mod2", 0, 1),
            ModuleSvg::Unavailable("svg_trace.mod2 が読み込まれていない")
        );
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
    }

    /// ビルドした mod2 を `svg_trace.mod2` という名前で読み込み、名前で見つけて入口を呼べること
    /// （本体と同じく `.mod2` の拡張子で読み込む）。ビルド後に走らせる:
    ///   cargo build --release -p svg-trace-mod2 && cargo test --release -p svg-trace-aux2 -- --ignored
    #[test]
    #[ignore]
    fn finds_built_mod2_by_file_name() {
        let dll = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("target")
            .join("release")
            .join("svg_trace.dll");
        assert!(dll.is_file(), "先に mod2 をビルドする: {}", dll.display());
        let dir = temp_dir("load_mod2");
        let module = dir.join("svg_trace.mod2");
        std::fs::copy(&dll, &module).unwrap();
        let wide: Vec<u16> = module
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        assert!(!unsafe { LoadLibraryW(wide.as_ptr()) }.is_null(), "mod2 を読み込めない");
        // 読み込まれていて入口もある。まだ何もトレースしていないので結果は無い
        assert_eq!(read_svg_from_module(MOD2_NAME, 0, 1), ModuleSvg::NotFound);
        assert_eq!(read_svg_from_module(MOD2_NAME, 1, 0), ModuleSvg::NotFound);
    }
}

aviutl2::register_generic_plugin!(SvgTraceAux2);
