use aviutl2::{
    anyhow::{self, Context},
    tracing,
};
use std::path::{Path, PathBuf};

static EDIT_HANDLE: aviutl2::generic::GlobalEditHandle = aviutl2::generic::GlobalEditHandle::new();

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

fn send_latest_to_svg_h() -> aviutl2::AnyResult<()> {
    EDIT_HANDLE.call_edit_section(|edit| -> aviutl2::AnyResult<()> {
        let export_dir = resolve_export_dir_in_edit(edit);
        let src = latest_svg_source(&export_dir);
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
        tracing::info!("Created SVG_H from {}", svg_path.display());
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

aviutl2::register_generic_plugin!(SvgTraceAux2);
