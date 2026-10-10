use aviutl2::{
    anyhow::{self, Context},
    tracing,
};
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
            if let Err(e) = send_latest_to_svg_h() {
                tracing::error!("SVG_Hへ送る failed: {e:#}");
            }
        });
        registry.register_export_menu("SVG_TRACE\\最新SVGを確認書き出し", || {
            if let Err(e) = confirm_latest_export() {
                tracing::error!("export confirm failed: {e:#}");
            }
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

/// mod2 がトレースのたびに上書きする受け渡しファイル（mod2 の `handoff_svg_path` と同じ場所）。
/// v0.1.3 から、「自動書き出し」が OFF のときは書き出し先に `latest.svg` ができないので、こちらを読む
fn handoff_svg_path() -> PathBuf {
    aviutl2::config::app_data_path()
        .join("Plugin")
        .join("svg_trace")
        .join("latest_trace.svg")
}

/// mod2 がインスタンス（オブジェクト ID）ごとに書く受け渡しファイルの置き場
/// （mod2 の `instance_svg_dir` と同じ場所。v0.1.3 までの mod2 は書かない）
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

/// 直前にトレースした SVG の置き場。受け渡しファイルを優先し、無ければ書き出し先の `latest.svg`
/// （v0.1.2 以前の mod2 が書いたもの）
fn latest_svg_source(export_dir: &Path) -> PathBuf {
    let handoff = handoff_svg_path();
    if handoff.is_file() {
        handoff
    } else {
        export_dir.join("latest.svg")
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

fn write_export_root_sidecar(dir: &Path) -> aviutl2::AnyResult<()> {
    let sidecar = export_root_sidecar_path();
    if let Some(parent) = sidecar.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&sidecar, dir.to_string_lossy().as_bytes())
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

fn confirm_latest_export() -> aviutl2::AnyResult<()> {
    EDIT_HANDLE.call_edit_section(|edit| -> aviutl2::AnyResult<()> {
        let export_dir = resolve_export_dir_in_edit(edit);
        let src = latest_svg_source(&export_dir);
        if !src.is_file() {
            anyhow::bail!(
                "最新SVGがありません。先に SVG_TRACE でトレースしてください: {}",
                src.display()
            );
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        std::fs::create_dir_all(&export_dir)?;
        let dst = export_dir.join(format!("trace_{stamp}.svg"));
        std::fs::copy(&src, &dst)
            .with_context(|| format!("copy {} → {}", src.display(), dst.display()))?;
        tracing::info!("Exported {}", dst.display());
        Ok(())
    })??;
    Ok(())
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

fn send_latest_to_svg_h() -> aviutl2::AnyResult<()> {
    EDIT_HANDLE.call_edit_section(|edit| -> aviutl2::AnyResult<()> {
        let export_dir = resolve_export_dir_in_edit(edit);

        // 右クリックしたオブジェクトのトレース結果（mod2 が obj.id ごとに書く）を探す。
        // 無ければ、v0.1.3 までと同じく全体で最後にトレースした結果を送り、そのことをログに出す
        let target = menu_target_object(edit).map(|h| (h, edit.get_object_id(h).unwrap_or(0)));
        let session_start = SESSION_START.get().copied().unwrap_or(SystemTime::UNIX_EPOCH);
        let picked = target.and_then(|(h, id)| {
            instance_svg_source(id, session_start).map(|p| (p, describe_object(edit, h, id)))
        });
        let (src, sent_what) = match picked {
            Some((p, what)) => (p, format!("{what} のトレース結果")),
            None => {
                let reason = match target {
                    Some((h, id)) => format!(
                        "{} のトレース結果が見つからない（SVG_TRACE でないか、この起動でまだ描かれていない）",
                        describe_object(edit, h, id)
                    ),
                    None => "対象のオブジェクトが分からない".to_string(),
                };
                (
                    latest_svg_source(&export_dir),
                    format!("全体で最後にトレースした結果（{reason}ため）"),
                )
            }
        };
        if !src.is_file() {
            anyhow::bail!("最新SVGがありません。先に SVG_TRACE でトレースしてください");
        }
        let svg =
            std::fs::read_to_string(&src).with_context(|| format!("read {}", src.display()))?;

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
        tracing::info!(
            "SVG_Hへ送る: {sent_what} を送った（{} → {}）",
            src.display(),
            svg_path.display()
        );
        Ok(())
    })??;
    Ok(())
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
}

aviutl2::register_generic_plugin!(SvgTraceAux2);
