//! トレース結果のインスタンスごとのキャッシュと、書き出し。
//!
//! v0.1.2 まではモジュール全体で 1 つの `last_svg` を持ち、キャッシュに当たったときに
//! それを描いていた。SVG_TRACE を 2 つ以上置くと、後からトレースした方の結果が
//! もう片方にも描かれうる。v0.1.3 から、インスタンス（Lua の `obj.id`）ごとに
//! スロットを持ち、入力のキー（入力元・パラメータ・大きさ）が一致したときだけ当てる。
//!
//! 書き出しも v0.1.2 までは「自動書き出し」に関係なくトレースのたびに行っていた。
//! 今は次のとおり。
//!
//! - 「SVG_Hへ送る」用の受け渡しファイル（`Plugin/svg_trace/latest_trace.svg`）は
//!   トレースのたびに 1 本だけ上書きする（内容が同じなら書かない）。プロジェクト側の
//!   書き出し先には置かない
//! - 名前付き SVG（`{画像名}_NNN.svg` / `layer{n}_{f}.svg`）と書き出し先の `latest.svg` は
//!   「自動書き出し」が ON のときだけ書く
//! - インスタンスごとの受け渡しファイル（`Plugin/svg_trace/traces/{obj.id}.svg`）も
//!   トレースのたびに書く（内容が同じなら書かない）。aux2 の「SVG_Hへ送る」が、右クリックした
//!   オブジェクトの ID（SDK の `get_object_id`）でこれを引く。ID はアプリ起動ごとに振り直されるので、
//!   aux2 は起動より前に書かれたファイルを使わない
//!
//! ## Lua へ返すポインタの寿命
//!
//! `render_cached` と `rasterize_svg` は画素のポインタを Lua へ返し、Lua はそれを
//! `obj.putpixeldata` に渡す。返した時点でモジュールの Mutex は外れているので、スクリプトが
//! 並列に走ると、別のオブジェクトの `store_result` がスロットを捨てて、Lua が
//! `obj.putpixeldata` を呼ぶ前に画素を解放しうる（v0.1.3 まで）。
//!
//! 今は画素を `Arc` で持ち、Lua へ返した画素はスレッドごとの「貸し出し」にも 1 つ持たせる。
//! スロットを捨てても、貸し出しが参照している間は解放されない。貸し出しは、同じスレッドが次に
//! ポインタを受け取るときに入れ替わる。1 つのスレッドの Lua は逐次に動くので、その時点で前の
//! `obj.putpixeldata` は終わっている。しばらく呼ばれないスレッドの貸し出しは `LEND_GRACE` の後に
//! 片付ける（ポインタを受け取ってから `obj.putpixeldata` までは同じ Lua の実行の中で、一瞬で済む）

use crate::trace::{rasterize_svg, write_export_svg_in, TraceConfig, TraceResult};
use anyhow::{Context, Result};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// 削除されたオブジェクトのスロットがいつまでも残らないように、古いものから捨てる
pub const MAX_SLOTS: usize = 32;

/// 貸し出しを片付けるまでの猶予。これより長く呼ばれていないスレッドの分だけ捨てる
pub const LEND_GRACE: Duration = Duration::from_secs(30);

/// 描いた画素（ストレートの RGBA）。Lua へ返す間は貸し出しが参照を持つ
pub type Raster = Arc<Vec<u8>>;

/// ファイル入力の元画像の状態。上書きされたら当てない
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceStamp {
    pub path: PathBuf,
    pub len: u64,
    pub modified: Option<SystemTime>,
}

impl SourceStamp {
    pub fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Self {
            path: path.to_path_buf(),
            len: meta.len(),
            modified: meta.modified().ok(),
        })
    }

    fn still_valid(&self) -> bool {
        Self::of(&self.path).as_ref() == Some(self)
    }
}

#[derive(Debug)]
struct Slot {
    key: String,
    svg: String,
    width: u32,
    height: u32,
    source: Option<SourceStamp>,
    /// 描いた画素（ストレートの RGBA）。描くまでは None
    raster: Option<(Raster, u32, u32)>,
    used: u64,
}

/// 書き出し先。テストでは一時フォルダを渡す
#[derive(Debug, Clone)]
pub struct ExportTargets {
    /// 名前付き SVG と `latest.svg` の置き場（プロジェクト隣の `SVG_export` など）
    pub export_dir: PathBuf,
    /// 「SVG_Hへ送る」用の受け渡しファイル
    pub handoff_path: PathBuf,
    /// インスタンスごとの受け渡しファイルの置き場（`{obj.id}.svg`）
    pub instance_dir: PathBuf,
}

/// `store_result` が何を書いたか
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StoreOutcome {
    pub handoff_written: bool,
    pub exported: Option<PathBuf>,
    /// インスタンスごとの受け渡しファイルを書いたなら、そのパス
    pub instance_written: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct TraceStore {
    slots: HashMap<String, Slot>,
    tick: u64,
    /// 直前にトレースした結果（`get_last_svg` と、引数無しの `rasterize_svg` 用）
    last: Option<TraceResult>,
    /// 直前に受け渡しファイルへ書いた内容のハッシュ
    handoff_hash: Option<u64>,
    /// インスタンスごとの受け渡しファイルへ直前に書いた内容のハッシュ
    instance_hashes: HashMap<String, u64>,
    /// Lua へ返した画素。キーはスレッド ID（`GetCurrentThreadId`）
    lent: HashMap<u32, (Raster, Instant)>,
}

fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// インスタンス名（Lua の `tostring(obj.id)`）を受け渡しファイルの名前にする。
///
/// aux2 は SDK の `get_object_id`（i64）を 10 進で書いた名前で引くので、数値なら整数の
/// 10 進に揃える（LuaJIT の `tostring` は大きな数を `1e+15` の形で書くため）。
/// 数値でなければ英数字と `_` `-` だけのときに限ってそのまま使い、それ以外は書かない
pub fn instance_file_stem(instance: &str) -> Option<String> {
    let s = instance.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(v) = s.parse::<f64>() {
        const EXACT: f64 = 9_007_199_254_740_992.0; // 2^53
        if v.is_finite() && v.fract() == 0.0 && v.abs() <= EXACT {
            return Some(format!("{}", v as i64));
        }
        return None;
    }
    let safe = s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    safe.then(|| s.to_string())
}

impl TraceStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// トレース結果をインスタンスのスロットに入れ、受け渡しファイルと（ON なら）名前付き SVG を書く
    pub fn store_result(
        &mut self,
        result: &TraceResult,
        cfg: &TraceConfig,
        source: Option<SourceStamp>,
        targets: &ExportTargets,
    ) -> Result<StoreOutcome> {
        let used = self.next_tick();
        self.slots.insert(
            cfg.instance.clone(),
            Slot {
                key: cfg.cache_key.clone(),
                svg: result.svg.clone(),
                width: result.width,
                height: result.height,
                source,
                raster: None,
                used,
            },
        );
        self.evict_old(&cfg.instance);
        self.last = Some(result.clone());

        let mut outcome = StoreOutcome::default();
        let h = hash_str(&result.svg);
        if self.handoff_hash != Some(h) {
            write_file(&targets.handoff_path, &result.svg)?;
            self.handoff_hash = Some(h);
            outcome.handoff_written = true;
        }
        if let Some(stem) = instance_file_stem(&cfg.instance) {
            if self.instance_hashes.get(&stem) != Some(&h) {
                let path = targets.instance_dir.join(format!("{stem}.svg"));
                match write_file(&path, &result.svg) {
                    Ok(()) => {
                        self.instance_hashes.insert(stem, h);
                        outcome.instance_written = Some(path);
                    }
                    Err(_) => {
                        // 「SVG_Hへ送る」のためだけのファイルなので、描画は止めない。
                        // 前のトレース結果を残すと aux2 が古い内容を送るので、消して「無い」にする
                        // （aux2 は直前のトレース結果へ戻り、そのことをログに出す）
                        let _ = std::fs::remove_file(&path);
                        self.instance_hashes.remove(&stem);
                    }
                }
            }
        }
        if cfg.auto_export {
            let path = write_export_svg_in(&targets.export_dir, &result.svg, &cfg.export_meta())?;
            outcome.exported = Some(path);
        }
        Ok(outcome)
    }

    fn evict_old(&mut self, keep: &str) {
        while self.slots.len() > MAX_SLOTS {
            let oldest = self
                .slots
                .iter()
                .filter(|(k, _)| k.as_str() != keep)
                .min_by_key(|(_, s)| s.used)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    self.slots.remove(&k);
                }
                None => break,
            }
        }
    }

    /// インスタンスのスロットが `key` の結果を持っていれば、描いた画素を返す。
    ///
    /// Lua へポインタを返すときは、返す前に `lend` で貸し出しに入れる。スロットが持つ参照は
    /// そのインスタンスが次にトレースするか、スロットが捨てられると外れる
    pub fn render_cached(&mut self, instance: &str, key: &str) -> Result<Option<(Raster, u32, u32)>> {
        let valid = match self.slots.get(instance) {
            Some(slot) => {
                slot.key == key && slot.source.as_ref().is_none_or(SourceStamp::still_valid)
            }
            None => return Ok(None),
        };
        if !valid {
            return Ok(None);
        }
        let used = self.next_tick();
        let slot = self.slots.get_mut(instance).expect("checked above");
        slot.used = used;
        if slot.raster.is_none() {
            let (buf, w, h) = rasterize_svg(&slot.svg)?;
            slot.raster = Some((Arc::new(buf), w, h));
        }
        let (buf, w, h) = slot.raster.as_ref().expect("filled above");
        Ok(Some((Arc::clone(buf), *w, *h)))
    }

    /// Lua へ返す画素を、呼び出したスレッドの貸し出しに入れてポインタを返す。
    ///
    /// そのスレッドの前の貸し出しはここで外れる（同じスレッドの Lua は逐次に動くので、
    /// 前に返したポインタの `obj.putpixeldata` は終わっている）。ほかのスレッドの貸し出しは
    /// `LEND_GRACE` より古いものだけ片付ける
    pub fn lend(&mut self, thread: u32, raster: Raster, now: Instant) -> *const u8 {
        let ptr = raster.as_ptr();
        self.lent.insert(thread, (raster, now));
        self.lent
            .retain(|t, (_, at)| *t == thread || now.saturating_duration_since(*at) < LEND_GRACE);
        ptr
    }

    /// インスタンスの SVG（無ければ直前のトレース結果）
    pub fn svg_for(&self, instance: Option<&str>) -> Option<(String, u32, u32)> {
        if let Some(slot) = instance.and_then(|i| self.slots.get(i)) {
            return Some((slot.svg.clone(), slot.width, slot.height));
        }
        self.last
            .as_ref()
            .map(|r| (r.svg.clone(), r.width, r.height))
    }

    pub fn last(&self) -> Option<&TraceResult> {
        self.last.as_ref()
    }

    #[cfg(test)]
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }
}

fn write_file(path: &Path, svg: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, svg.as_bytes()).with_context(|| format!("Failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::vectorize_rgba;
    use image::{Rgba, RgbaImage};

    fn temp_targets(name: &str) -> ExportTargets {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("store_test")
            .join(name);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        ExportTargets {
            export_dir: root.join("SVG_export"),
            handoff_path: root.join("plugin").join("latest_trace.svg"),
            instance_dir: root.join("plugin").join("traces"),
        }
    }

    fn rect_svg(color: &str) -> TraceResult {
        TraceResult {
            svg: format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16" fill="{color}"/></svg>"#
            ),
            width: 16,
            height: 16,
        }
    }

    fn cfg(instance: &str, key: &str, auto_export: bool) -> TraceConfig {
        TraceConfig {
            instance: instance.into(),
            cache_key: key.into(),
            auto_export,
            export_stem: Some("logo".into()),
            ..TraceConfig::default()
        }
    }

    fn center_pixel(buf: &[u8], w: u32, h: u32) -> [u8; 4] {
        assert_eq!(buf.len(), (w * h * 4) as usize);
        let i = (((h / 2) * w + w / 2) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    fn files_in(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    /// 1. 自動書き出し OFF では書き出し先に何も作らない（受け渡しファイルだけ）
    #[test]
    fn auto_export_off_writes_nothing_to_export_dir() {
        let t = temp_targets("off");
        let mut store = TraceStore::new();
        for (i, color) in ["#ff0000", "#00ff00", "#0000ff"].iter().enumerate() {
            let out = store
                .store_result(&rect_svg(color), &cfg("A", &format!("k{i}"), false), None, &t)
                .unwrap();
            assert_eq!(out.exported, None);
            assert!(out.handoff_written);
        }
        assert!(
            !t.export_dir.exists(),
            "OFF なのに書き出し先ができた: {:?}",
            files_in(&t.export_dir)
        );
        let handoff = std::fs::read_to_string(&t.handoff_path).unwrap();
        assert!(handoff.contains("#0000ff"), "受け渡しファイルは最後のトレース結果");
    }

    /// 1. 自動書き出し ON なら従来どおり連番と latest.svg を書く
    #[test]
    fn auto_export_on_writes_named_files() {
        let t = temp_targets("on");
        let mut store = TraceStore::new();
        store
            .store_result(&rect_svg("#ff0000"), &cfg("A", "k0", true), None, &t)
            .unwrap();
        let out = store
            .store_result(&rect_svg("#00ff00"), &cfg("A", "k1", true), None, &t)
            .unwrap();
        assert_eq!(out.exported, Some(t.export_dir.join("logo_001.svg")));
        assert_eq!(
            files_in(&t.export_dir),
            ["latest.svg", "logo_000.svg", "logo_001.svg"]
        );
    }

    /// 同じ内容なら受け渡しファイルを書き直さない
    #[test]
    fn handoff_skips_identical_content() {
        let t = temp_targets("same");
        let mut store = TraceStore::new();
        let r = rect_svg("#ff0000");
        assert!(store.store_result(&r, &cfg("A", "k", false), None, &t).unwrap().handoff_written);
        assert!(!store.store_result(&r, &cfg("B", "k", false), None, &t).unwrap().handoff_written);
    }

    /// 2. 2 インスタンスがそれぞれの結果を描く（v0.1.2 は後からトレースした方の SVG を両方に描いた）
    #[test]
    fn two_instances_do_not_mix() {
        let t = temp_targets("mix");
        let mut store = TraceStore::new();
        store
            .store_result(&rect_svg("#ff0000"), &cfg("A", "file|a.png", false), None, &t)
            .unwrap();
        store
            .store_result(&rect_svg("#0000ff"), &cfg("B", "file|b.png", false), None, &t)
            .unwrap();
        // v0.1.2 の描き方（直前の結果）では A も青になる。検査が空振りしていないことの確認
        assert!(store.last().unwrap().svg.contains("#0000ff"));

        let (p, w, h) = store.render_cached("A", "file|a.png").unwrap().expect("A hit");
        assert_eq!(center_pixel(&p, w, h), [255, 0, 0, 255], "A に B の結果が描かれた");
        let (p, w, h) = store.render_cached("B", "file|b.png").unwrap().expect("B hit");
        assert_eq!(center_pixel(&p, w, h), [0, 0, 255, 255]);
        // 2 回目は描いた画素を使い回しても同じ
        let (p, w, h) = store.render_cached("A", "file|a.png").unwrap().expect("A hit again");
        assert_eq!(center_pixel(&p, w, h), [255, 0, 0, 255]);

        // 入力のキーが違えば当てない。知らないインスタンスも当てない
        assert!(store.render_cached("A", "file|b.png").unwrap().is_none());
        assert!(store.render_cached("C", "file|a.png").unwrap().is_none());
        // write_latest 用の SVG もインスタンスごと
        assert!(store.svg_for(Some("A")).unwrap().0.contains("#ff0000"));
    }

    /// 2. 実際のトレース結果でも混ざらない（vectorize → store → render）
    #[test]
    fn two_instances_real_trace() {
        let t = temp_targets("real");
        let mut store = TraceStore::new();
        let make = |c: [u8; 4]| {
            let mut img = RgbaImage::from_pixel(32, 32, Rgba([0, 0, 0, 0]));
            for y in 4..28 {
                for x in 4..28 {
                    img.put_pixel(x, y, Rgba(c));
                }
            }
            img
        };
        let color = |inst: &str, key: &str| TraceConfig {
            mode: 1,
            colors: 4,
            ..cfg(inst, key, false)
        };
        let a = vectorize_rgba(make([220, 30, 30, 255]).as_raw(), 32, 32, &color("A", "ka")).unwrap();
        let b = vectorize_rgba(make([30, 30, 220, 255]).as_raw(), 32, 32, &color("B", "kb")).unwrap();
        store.store_result(&a, &color("A", "ka"), None, &t).unwrap();
        store.store_result(&b, &color("B", "kb"), None, &t).unwrap();
        let (p, w, h) = store.render_cached("A", "ka").unwrap().unwrap();
        let px = center_pixel(&p, w, h);
        assert!(px[0] > 180 && px[2] < 80, "A が赤でない: {px:?}");
        let (p, w, h) = store.render_cached("B", "kb").unwrap().unwrap();
        let px = center_pixel(&p, w, h);
        assert!(px[2] > 180 && px[0] < 80, "B が青でない: {px:?}");
    }

    /// ファイル入力は元画像が書き換わったら当てない
    #[test]
    fn file_source_change_misses() {
        let t = temp_targets("stamp");
        let src = t.handoff_path.parent().unwrap().parent().unwrap().join("src.png");
        std::fs::write(&src, b"1234").unwrap();
        let mut store = TraceStore::new();
        store
            .store_result(&rect_svg("#ff0000"), &cfg("A", "k", false), SourceStamp::of(&src), &t)
            .unwrap();
        assert!(store.render_cached("A", "k").unwrap().is_some());
        std::fs::write(&src, b"123456").unwrap();
        assert!(store.render_cached("A", "k").unwrap().is_none());
    }

    /// スロットは MAX_SLOTS を超えたら古いものから捨てる
    #[test]
    fn slots_are_bounded() {
        let t = temp_targets("bound");
        let mut store = TraceStore::new();
        for i in 0..(MAX_SLOTS + 5) {
            store
                .store_result(&rect_svg("#ff0000"), &cfg(&format!("I{i}"), "k", false), None, &t)
                .unwrap();
        }
        assert_eq!(store.slot_count(), MAX_SLOTS);
        assert!(store.render_cached("I0", "k").unwrap().is_none());
        assert!(store.render_cached(&format!("I{}", MAX_SLOTS + 4), "k").unwrap().is_some());
    }

    /// Lua へ貸した画素は、スロットが入れ替わっても捨てられても、同じスレッドが次に
    /// 受け取るまで解放されない（v0.1.3 はスロットを捨てた時点で解放していた）
    #[test]
    fn lent_raster_outlives_slot_replacement() {
        let t = temp_targets("lend");
        let mut store = TraceStore::new();
        let t0 = Instant::now();
        store
            .store_result(&rect_svg("#ff0000"), &cfg("A", "k0", false), None, &t)
            .unwrap();
        let (raster, w, h) = store.render_cached("A", "k0").unwrap().unwrap();
        let weak = Arc::downgrade(&raster);
        let ptr = store.lend(1, raster, t0);

        // 別のオブジェクトの処理に見立てて、A のスロットを入れ替え、さらに追い出す
        store
            .store_result(&rect_svg("#0000ff"), &cfg("A", "k1", false), None, &t)
            .unwrap();
        for i in 0..(MAX_SLOTS + 2) {
            store
                .store_result(&rect_svg("#00ff00"), &cfg(&format!("X{i}"), "k", false), None, &t)
                .unwrap();
        }
        assert!(store.render_cached("A", "k1").unwrap().is_none(), "A は追い出されているはず");
        assert!(weak.upgrade().is_some(), "貸した画素が解放された");
        let lent = unsafe { std::slice::from_raw_parts(ptr, (w * h * 4) as usize) };
        assert_eq!(center_pixel(lent, w, h), [255, 0, 0, 255]);

        // 同じスレッドが次の画素を受け取ったら、前の貸し出しは外れる
        store
            .store_result(&rect_svg("#0000ff"), &cfg("A", "k1", false), None, &t)
            .unwrap();
        let (next, _, _) = store.render_cached("A", "k1").unwrap().unwrap();
        store.lend(1, next, t0);
        assert!(weak.upgrade().is_none(), "前の貸し出しが残り続けている");
    }

    /// ほかのスレッドの貸し出しは、猶予を過ぎるまで片付けない
    #[test]
    fn lend_keeps_other_threads_until_grace() {
        let mut store = TraceStore::new();
        let t0 = Instant::now();
        let a: Raster = Arc::new(vec![1; 4]);
        let weak_a = Arc::downgrade(&a);
        store.lend(1, a, t0);
        store.lend(2, Arc::new(vec![2; 4]), t0 + Duration::from_secs(1));
        assert!(weak_a.upgrade().is_some(), "猶予の内にほかのスレッドの貸し出しを捨てた");
        store.lend(2, Arc::new(vec![3; 4]), t0 + LEND_GRACE + Duration::from_secs(1));
        assert!(weak_a.upgrade().is_none(), "猶予を過ぎた貸し出しが残っている");
    }

    /// 並列に走らせても、受け取った画素が読む前に別の内容へ変わらない
    #[test]
    fn parallel_render_reads_own_pixels() {
        use std::sync::Mutex;
        let t = temp_targets("parallel");
        let store = Arc::new(Mutex::new(TraceStore::new()));
        let colors = [("#ff0000", [255, 0, 0, 255]), ("#00ff00", [0, 255, 0, 255]), ("#0000ff", [0, 0, 255, 255])];
        let handles: Vec<_> = colors
            .iter()
            .enumerate()
            .map(|(n, (color, expect))| {
                let store = Arc::clone(&store);
                let t = t.clone();
                let color = color.to_string();
                let expect = *expect;
                std::thread::spawn(move || {
                    let thread = n as u32 + 1;
                    for i in 0..200 {
                        let inst = format!("T{n}");
                        let key = format!("k{i}");
                        let (ptr, w, h) = {
                            let mut s = store.lock().unwrap();
                            s.store_result(&rect_svg(&color), &cfg(&inst, &key, false), None, &t)
                                .unwrap();
                            // ほかのスレッドのスロットを追い出す
                            for j in 0..(MAX_SLOTS / 2) {
                                s.store_result(
                                    &rect_svg("#ffffff"),
                                    &cfg(&format!("F{n}_{j}"), "k", false),
                                    None,
                                    &t,
                                )
                                .unwrap();
                            }
                            let (r, w, h) = s.render_cached(&inst, &key).unwrap().unwrap();
                            (s.lend(thread, r, Instant::now()), w, h)
                        };
                        // Mutex の外で読む（Lua が obj.putpixeldata を呼ぶのと同じ）
                        std::thread::yield_now();
                        let px = unsafe { std::slice::from_raw_parts(ptr, (w * h * 4) as usize) };
                        assert_eq!(center_pixel(px, w, h), expect);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    /// インスタンスごとの受け渡しファイル（aux2 が右クリックしたオブジェクトの ID で引く）
    #[test]
    fn instance_handoff_files() {
        let t = temp_targets("instance");
        let mut store = TraceStore::new();
        let out = store
            .store_result(&rect_svg("#ff0000"), &cfg("12345", "k", false), None, &t)
            .unwrap();
        let p = t.instance_dir.join("12345.svg");
        assert_eq!(out.instance_written.as_deref(), Some(p.as_path()));
        assert!(std::fs::read_to_string(&p).unwrap().contains("#ff0000"));

        // 同じ内容なら書き直さない。別のインスタンスは別のファイル
        let out = store
            .store_result(&rect_svg("#ff0000"), &cfg("12345", "k2", false), None, &t)
            .unwrap();
        assert_eq!(out.instance_written, None);
        store
            .store_result(&rect_svg("#0000ff"), &cfg("67890", "k", false), None, &t)
            .unwrap();
        assert!(std::fs::read_to_string(&p).unwrap().contains("#ff0000"), "別のインスタンスに上書きされた");
        assert!(std::fs::read_to_string(t.instance_dir.join("67890.svg")).unwrap().contains("#0000ff"));

        // 名前にできないインスタンスは書かない
        let out = store
            .store_result(&rect_svg("#00ff00"), &cfg("../evil", "k", false), None, &t)
            .unwrap();
        assert_eq!(out.instance_written, None);
        assert_eq!(files_in(&t.instance_dir), ["12345.svg", "67890.svg"]);
    }

    #[test]
    fn instance_file_stem_matches_object_id() {
        assert_eq!(instance_file_stem("42").as_deref(), Some("42"));
        assert_eq!(instance_file_stem(" 42 ").as_deref(), Some("42"));
        // LuaJIT の tostring は 1e15 以上を指数で書く
        assert_eq!(instance_file_stem("1e+15").as_deref(), Some("1000000000000000"));
        assert_eq!(instance_file_stem("-7").as_deref(), Some("-7"));
        assert_eq!(instance_file_stem("1.5"), None);
        assert_eq!(instance_file_stem("inf"), None);
        assert_eq!(instance_file_stem(""), None);
        assert_eq!(instance_file_stem("abc_1").as_deref(), Some("abc_1"));
        assert_eq!(instance_file_stem("a/b"), None);
        assert_eq!(instance_file_stem("..\\x"), None);
    }
}
