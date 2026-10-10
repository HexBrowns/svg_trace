//! トレース結果のインスタンスごとのキャッシュと、書き出し。
//!
//! v0.1.2 まではモジュール全体で 1 つの `last_svg` を持ち、キャッシュに当たったときに
//! それを描いていた。SVG_TRACE を 2 つ以上置くと、後からトレースした方の結果が
//! もう片方にも描かれうる。v0.1.3 から、インスタンス（Lua の `obj.id`）ごとに
//! スロットを持ち、入力のキー（入力元・パラメータ・大きさ）が一致したときだけ当てる。
//!
//! ## 書き出し（v0.2.0）
//!
//! - **「自動書き出し」が OFF なら、ファイルを 1 つも書かない。** v0.1.4 までは「SVG_Hへ送る」のために
//!   `Plugin/svg_trace/latest_trace.svg` と `traces/{obj.id}.svg` をトレースのたびに書いていた
//!   （レイヤー入力で絵が動くと毎フレーム 2 本）。今は aux2 が、同じプロセスに読み込まれたこのモジュールから
//!   `svg_trace_copy_svg_v1`（`lib.rs`）でメモリの結果を直接読む
//! - ON なら、名前付き SVG（`{画像名}_{連番}.svg` / `layer{n}_{f}.svg`）と書き出し先の `latest.svg` を書く。
//!   内容が前に書いたものと同じなら書かない。連番は、同じオブジェクトが `EXPORT_COALESCE` の内に続けて
//!   トレースし直した間（スライダーのドラッグ）は同じ番号を上書きし、間が空いたら次の番号にする
//! - **ファイルの読み書きと SVG の描画（ラスタライズ）は Mutex の外で行う**（v0.1.4 までは中で行っていて、
//!   ほかのオブジェクトの描画を待たせていた）。Mutex の中では、何を書くかを決めるだけ
//!
//! ## キャッシュの大きさ
//!
//! スロットは `MAX_SLOTS` 個まで、描いた画素と SVG と読んだ画像を合わせて `MAX_BYTES` まで。超えたら古いものから
//! 捨てる（描いた画素は SVG から描き直せるので先に捨てる）。`RASTER_IDLE` の間使われていない描いた画素と
//! 読んだ画像も捨てる（削除したオブジェクトの分が残り続けないように）。捨てたものが直前まで使われていた
//! （同時に見えているものが上限を超え、毎フレーム入れ替わっている）ときは、原因ごとに 1 回だけ警告をログに出す
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

use crate::trace::{self, numbered_file_name, rasterize_svg, ExportName, TraceConfig, TraceResult};
use anyhow::{Context, Result};
use image::RgbaImage;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

/// スロットの数の上限。同時に見えている SVG_TRACE がこれを超えると、毎フレーム入れ替わってトレースし直す
pub const MAX_SLOTS: usize = 32;

/// 描いた画素・SVG・読んだ画像を合わせたバイト数の上限（256 MB。FHD の描いた画素なら 30 枚ほど）
pub const MAX_BYTES: usize = 256 << 20;

/// これより長く使われていない描いた画素と読んだ画像は捨てる（SVG は残すので、トレースし直しにはならない）
pub const RASTER_IDLE: Duration = Duration::from_secs(60);

/// 捨てたものがこれより短い間に使われていたら、「入れ替わり続けている」とみなして警告する
pub const THRASH_WINDOW: Duration = Duration::from_secs(2);

/// 同じオブジェクトがこの間に続けてトレースし直したら、自動書き出しの連番を進めずに同じファイルを上書きする
pub const EXPORT_COALESCE: Duration = Duration::from_secs(2);

/// 読んだ画像（ファイル入力）を持っておく数。スライダーを動かすたびに画像を読み直さないため
pub const MAX_DECODED: usize = 4;

/// 貸し出しを片付けるまでの猶予。これより長く呼ばれていないスレッドの分だけ捨てる
pub const LEND_GRACE: Duration = Duration::from_secs(30);

/// 書いたファイルの内容のハッシュを覚えておく数（超えたら忘れる。忘れても 1 回余計に書くだけ）
const MAX_WRITTEN: usize = 1024;

/// 描いた画素（ストレートの RGBA）。Lua へ返す間は貸し出しが参照を持つ
pub type Raster = Arc<Vec<u8>>;

/// SVG の文字列。スロットと書き出しで共有する
pub type Svg = Arc<str>;

/// モジュール全体のキャッシュ。aux2 も `svg_trace_copy_svg_v1` を通してこれを読む
pub static STORE: LazyLock<SharedStore> = LazyLock::new(SharedStore::new);

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

    pub fn still_valid(&self) -> bool {
        Self::of(&self.path).as_ref() == Some(self)
    }
}

/// スロットの中身。失敗も覚えておき、同じ入力で毎フレームトレースし直さない
#[derive(Debug, Clone)]
enum Content {
    Ok { svg: Svg, width: u32, height: u32 },
    Failed(String),
}

#[derive(Debug)]
struct Slot {
    key: String,
    content: Content,
    source: Option<SourceStamp>,
    /// 描いた画素（ストレートの RGBA）。描くまでは None
    raster: Option<(Raster, u32, u32)>,
    used: u64,
    used_at: Instant,
    /// スロットを入れ替えるたびに増える。Mutex の外で描いた画素を、入れ替わった後のスロットに付けない
    generation: u64,
}

impl Slot {
    fn bytes(&self) -> usize {
        let svg = match &self.content {
            Content::Ok { svg, .. } => svg.len(),
            Content::Failed(m) => m.len(),
        };
        self.key.len() + svg + self.raster.as_ref().map_or(0, |(r, _, _)| r.len())
    }
}

/// 読んだ画像（長辺 `MAX_SIDE` に縮めた後）
struct Decoded {
    stamp: SourceStamp,
    image: Arc<RgbaImage>,
    used_at: Instant,
}

/// 連番の書き出しの状態（書き出し先と画像名ごと）
#[derive(Debug)]
struct SeqState {
    next: u32,
    last: Option<LastNumbered>,
}

#[derive(Debug)]
struct LastNumbered {
    path: PathBuf,
    instance: String,
    at: Instant,
    hash: u64,
}

/// `render_cached` の結果
#[derive(Debug)]
pub enum Cached {
    /// そのインスタンスに、このキーの結果が無い（トレースする）
    Miss,
    /// このキーではトレースに失敗した（トレースし直さない）
    Failed(String),
    Hit(Raster, u32, u32),
}

impl Cached {
    #[cfg(test)]
    pub fn hit(self) -> Option<(Raster, u32, u32)> {
        match self {
            Cached::Hit(r, w, h) => Some((r, w, h)),
            _ => None,
        }
    }
}

/// `lookup` が Mutex の中で取り出すもの。描く・元画像を確かめるのは Mutex の外で行う
struct View {
    content: Content,
    raster: Option<(Raster, u32, u32)>,
    source: Option<SourceStamp>,
    generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WriteKind {
    Overwrite,
    /// 新しい連番。同じ名前のファイルが既にあれば次の番号にする
    NewNumbered { dir: PathBuf, stem: String },
}

#[derive(Debug)]
struct PlannedWrite {
    path: PathBuf,
    kind: WriteKind,
    is_latest: bool,
}

/// `store_result` が何を書いたか
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StoreOutcome {
    /// 名前付き SVG を書いたなら、そのパス
    pub exported: Option<PathBuf>,
    /// 書き出し先の `latest.svg` を書いたか
    pub latest_written: bool,
    /// 書けなかったもの（描画は止めない。ログに 1 回出す）
    pub errors: Vec<String>,
}

#[derive(Default)]
pub struct TraceStore {
    slots: HashMap<String, Slot>,
    tick: u64,
    generation: u64,
    /// 直前にトレースした結果（`get_last_svg`、引数無しの `rasterize_svg`、aux2 の「最新SVGを確認書き出し」）
    last: Option<(Svg, u32, u32)>,
    /// 書いたファイルの内容のハッシュ。同じ内容なら書き直さない
    written: HashMap<PathBuf, u64>,
    sequences: HashMap<(PathBuf, String), SeqState>,
    /// Lua へ返した画素。キーはスレッド ID（`GetCurrentThreadId`）
    lent: HashMap<u32, (Raster, Instant)>,
    decoded: Vec<Decoded>,
    /// 警告を出した原因（原因ごとに 1 回だけ出す）
    warned: HashSet<&'static str>,
    /// まだログに出していない警告（Mutex を外してから出す）
    warnings: Vec<String>,
}

fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// インスタンス名（Lua の `tostring(obj.id)`）を、aux2 が引くオブジェクト ID の 10 進の形にする。
///
/// aux2 は SDK の `get_object_id`（i64）で引くので、数値なら整数の 10 進に揃える
/// （LuaJIT の `tostring` は大きな数を `1e+15` の形で書くため）。
/// 数値でなければ英数字と `_` `-` だけのときに限ってそのまま使い、それ以外は None
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

    fn warn_once(&mut self, cause: &'static str, msg: String) {
        if self.warned.insert(cause) {
            self.warnings.push(msg);
        }
    }

    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    /// トレース結果（失敗も）をインスタンスのスロットに入れる。成功なら SVG を返す
    fn insert_result(
        &mut self,
        result: std::result::Result<TraceResult, String>,
        cfg: &TraceConfig,
        source: Option<SourceStamp>,
        now: Instant,
    ) -> Option<Svg> {
        let used = self.next_tick();
        self.generation += 1;
        let (content, svg) = match result {
            Ok(r) => {
                let svg: Svg = Arc::from(r.svg);
                self.last = Some((svg.clone(), r.width, r.height));
                (
                    Content::Ok {
                        svg: svg.clone(),
                        width: r.width,
                        height: r.height,
                    },
                    Some(svg),
                )
            }
            Err(e) => (Content::Failed(e), None),
        };
        self.slots.insert(
            cfg.instance.clone(),
            Slot {
                key: cfg.cache_key.clone(),
                content,
                source,
                raster: None,
                used,
                used_at: now,
                generation: self.generation,
            },
        );
        self.sweep_idle(now);
        self.enforce_budget(&cfg.instance, now);
        svg
    }

    /// インスタンスのスロットが `key` のものなら、使った印を付けて中身を返す
    fn lookup(&mut self, instance: &str, key: &str, now: Instant) -> Option<View> {
        let used = self.next_tick();
        let slot = self.slots.get_mut(instance).filter(|s| s.key == key)?;
        slot.used = used;
        slot.used_at = now;
        let view = View {
            content: slot.content.clone(),
            raster: slot.raster.clone(),
            source: slot.source.clone(),
            generation: slot.generation,
        };
        self.sweep_idle(now);
        Some(view)
    }

    /// Mutex の外で描いた画素を付ける。その間にスロットが入れ替わっていたら付けない
    fn attach_raster(&mut self, instance: &str, generation: u64, raster: (Raster, u32, u32), now: Instant) {
        if let Some(slot) = self.slots.get_mut(instance) {
            if slot.generation == generation && slot.raster.is_none() {
                slot.raster = Some(raster);
                self.enforce_budget(instance, now);
            }
        }
    }

    /// 描けなかった SVG は、同じ入力の間は失敗として覚える
    fn mark_failed(&mut self, instance: &str, generation: u64, msg: &str) {
        if let Some(slot) = self.slots.get_mut(instance) {
            if slot.generation == generation {
                slot.content = Content::Failed(msg.to_string());
            }
        }
    }

    /// 使われていない描いた画素と読んだ画像を捨てる
    fn sweep_idle(&mut self, now: Instant) {
        for slot in self.slots.values_mut() {
            if slot.raster.is_some() && now.saturating_duration_since(slot.used_at) >= RASTER_IDLE {
                slot.raster = None;
            }
        }
        self.decoded
            .retain(|d| now.saturating_duration_since(d.used_at) < RASTER_IDLE);
    }

    pub fn total_bytes(&self) -> usize {
        self.slots.values().map(Slot::bytes).sum::<usize>()
            + self.decoded.iter().map(|d| d.image.as_raw().len()).sum::<usize>()
    }

    /// `keep` 以外で一番古く使われたスロット。`with_raster` なら描いた画素を持つものだけ
    fn lru_slot(&self, keep: &str, with_raster: bool) -> Option<String> {
        self.slots
            .iter()
            .filter(|(k, s)| k.as_str() != keep && (!with_raster || s.raster.is_some()))
            .min_by_key(|(_, s)| s.used)
            .map(|(k, _)| k.clone())
    }

    fn enforce_budget(&mut self, keep: &str, now: Instant) {
        let recent = |at: Instant| now.saturating_duration_since(at) < THRASH_WINDOW;
        while self.slots.len() > MAX_SLOTS {
            let Some(k) = self.lru_slot(keep, false) else {
                break;
            };
            let slot = self.slots.remove(&k).expect("listed above");
            if recent(slot.used_at) {
                self.warn_once(
                    "slots",
                    format!(
                        "SVG_TRACE: 同時に描いている SVG_TRACE が {MAX_SLOTS} 個を超えたため、キャッシュから外れたものを毎回トレースし直している（重くなる）。同時に見える数を減らしてください"
                    ),
                );
            }
        }
        while self.total_bytes() > MAX_BYTES {
            // 描いた画素は SVG から描き直せるので先に捨てる。次に読んだ画像、最後にスロットごと
            if let Some(k) = self.lru_slot(keep, true) {
                let slot = self.slots.get_mut(&k).expect("listed above");
                slot.raster = None;
                if recent(slot.used_at) {
                    self.warn_bytes();
                }
                continue;
            }
            if let Some(i) = (0..self.decoded.len()).min_by_key(|&i| self.decoded[i].used_at) {
                self.decoded.remove(i);
                continue;
            }
            let Some(k) = self.lru_slot(keep, false) else {
                break;
            };
            let slot = self.slots.remove(&k).expect("listed above");
            if recent(slot.used_at) {
                self.warn_bytes();
            }
        }
    }

    fn warn_bytes(&mut self) {
        self.warn_once(
            "bytes",
            format!(
                "SVG_TRACE: トレース結果のキャッシュが上限（{} MB）を超えたため、毎回描き直している（重くなる）。同時に見える SVG_TRACE を減らすか、小さい画像にしてください",
                MAX_BYTES >> 20
            ),
        );
    }

    fn knows_sequence(&self, dir: &Path, stem: &str) -> bool {
        self.sequences
            .contains_key(&(dir.to_path_buf(), stem.to_string()))
    }

    /// 書く前に呼ぶ。前に同じ内容を書いていれば false（書かない）
    fn mark_written(&mut self, path: &Path, hash: u64) -> bool {
        if self.written.get(path) == Some(&hash) {
            return false;
        }
        if self.written.len() >= MAX_WRITTEN {
            self.written.clear();
        }
        self.written.insert(path.to_path_buf(), hash);
        true
    }

    fn forget_written(&mut self, path: &Path) {
        self.written.remove(path);
    }

    /// 自動書き出しで何を書くかを決める（書くのは Mutex の外）
    fn plan_export(
        &mut self,
        cfg: &TraceConfig,
        svg: &Svg,
        dir: &Path,
        scanned: Option<u32>,
        now: Instant,
    ) -> Vec<PlannedWrite> {
        let h = hash_str(svg);
        let mut writes = Vec::new();
        match cfg.export_meta().export_name() {
            ExportName::Fixed(name) => {
                let path = dir.join(name);
                if self.mark_written(&path, h) {
                    writes.push(PlannedWrite {
                        path,
                        kind: WriteKind::Overwrite,
                        is_latest: false,
                    });
                }
            }
            ExportName::Numbered(stem) => {
                let st = self
                    .sequences
                    .entry((dir.to_path_buf(), stem.clone()))
                    .or_insert(SeqState { next: 0, last: None });
                if let Some(n) = scanned {
                    st.next = st.next.max(n);
                }
                let last_numbered = |path: PathBuf| LastNumbered {
                    path,
                    instance: cfg.instance.clone(),
                    at: now,
                    hash: h,
                };
                match &st.last {
                    // 前に書いた番号と同じ内容なら、新しい番号を作らない
                    Some(l) if l.hash == h => {}
                    // 続けて変えている間（スライダーのドラッグ）は同じ番号を上書きする
                    Some(l)
                        if l.instance == cfg.instance
                            && now.saturating_duration_since(l.at) < EXPORT_COALESCE =>
                    {
                        let path = l.path.clone();
                        st.last = Some(last_numbered(path.clone()));
                        writes.push(PlannedWrite {
                            path,
                            kind: WriteKind::Overwrite,
                            is_latest: false,
                        });
                    }
                    _ => {
                        let n = st.next;
                        st.next = n.saturating_add(1);
                        let path = dir.join(numbered_file_name(&stem, n));
                        st.last = Some(last_numbered(path.clone()));
                        writes.push(PlannedWrite {
                            path,
                            kind: WriteKind::NewNumbered {
                                dir: dir.to_path_buf(),
                                stem,
                            },
                            is_latest: false,
                        });
                    }
                }
            }
            ExportName::LatestOnly => {}
        }
        let latest = dir.join("latest.svg");
        if self.mark_written(&latest, h) {
            writes.push(PlannedWrite {
                path: latest,
                kind: WriteKind::Overwrite,
                is_latest: true,
            });
        }
        writes
    }

    /// 連番のファイルが既にあったとき、フォルダを数え直した番号で取り直す
    fn renumber(&mut self, dir: &Path, stem: &str, scanned: u32) -> PathBuf {
        let st = self
            .sequences
            .entry((dir.to_path_buf(), stem.to_string()))
            .or_insert(SeqState { next: 0, last: None });
        let n = st.next.max(scanned);
        st.next = n.saturating_add(1);
        let path = dir.join(numbered_file_name(stem, n));
        if let Some(l) = st.last.as_mut() {
            l.path = path.clone();
        }
        path
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

    /// インスタンスの SVG（`instance` が None なら直前のトレース結果）
    pub fn svg_for(&self, instance: Option<&str>) -> Option<(Svg, u32, u32)> {
        match instance {
            Some(i) => match &self.slots.get(i)?.content {
                Content::Ok { svg, width, height } => Some((svg.clone(), *width, *height)),
                Content::Failed(_) => None,
            },
            None => self.last.clone(),
        }
    }

    /// aux2 の「SVG_Hへ送る」用。SDK のオブジェクト ID（`get_object_id`）のインスタンスの SVG
    pub fn svg_for_object(&self, object_id: i64) -> Option<Svg> {
        if object_id == 0 {
            return None;
        }
        let want = object_id.to_string();
        self.slots.iter().find_map(|(k, s)| match &s.content {
            Content::Ok { svg, .. } if instance_file_stem(k).as_deref() == Some(want.as_str()) => {
                Some(svg.clone())
            }
            _ => None,
        })
    }

    fn decoded_get(&mut self, stamp: &SourceStamp, now: Instant) -> Option<Arc<RgbaImage>> {
        let d = self.decoded.iter_mut().find(|d| &d.stamp == stamp)?;
        d.used_at = now;
        Some(d.image.clone())
    }

    fn decoded_put(&mut self, stamp: SourceStamp, image: Arc<RgbaImage>, now: Instant) {
        self.decoded.retain(|d| d.stamp.path != stamp.path);
        if self.decoded.len() >= MAX_DECODED {
            if let Some(i) = (0..self.decoded.len()).min_by_key(|&i| self.decoded[i].used_at) {
                self.decoded.remove(i);
            }
        }
        self.decoded.push(Decoded {
            stamp,
            image,
            used_at: now,
        });
        self.enforce_budget("", now);
    }

    #[cfg(test)]
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    #[cfg(test)]
    pub fn has_raster(&self, instance: &str) -> bool {
        self.slots.get(instance).is_some_and(|s| s.raster.is_some())
    }

    #[cfg(test)]
    pub fn warned(&self, cause: &str) -> bool {
        self.warned.contains(cause)
    }

    #[cfg(test)]
    pub fn decoded_count(&self) -> usize {
        self.decoded.len()
    }
}

/// `TraceStore` を Mutex で包んだもの。読み書きと描画を Mutex の外で行う手順はここにまとめる
pub struct SharedStore(Mutex<TraceStore>);

impl Default for SharedStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedStore {
    pub fn new() -> Self {
        Self(Mutex::new(TraceStore::new()))
    }

    fn lock(&self) -> MutexGuard<'_, TraceStore> {
        // 中で panic しても、キャッシュが使えなくなるだけにする（毎フレーム失敗し続けない）
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Mutex の中で `f` を呼ぶ。たまった警告は Mutex を外してからログに出す
    pub fn with<T>(&self, f: impl FnOnce(&mut TraceStore) -> T) -> T {
        let (r, warnings) = {
            let mut s = self.lock();
            let r = f(&mut s);
            (r, s.take_warnings())
        };
        for w in warnings {
            aviutl2::lprintln!(warn, "{w}");
        }
        r
    }

    /// インスタンスのスロットが `key` の結果を持っていれば、描いた画素を返す。
    ///
    /// 元画像の確かめ（ファイル入力）と、描いていなければ描くのは Mutex の外で行う。
    /// Lua へポインタを返すときは、返す前に `lend` で貸し出しに入れる
    pub fn render_cached(&self, instance: &str, key: &str, now: Instant) -> Cached {
        let Some(view) = self.with(|s| s.lookup(instance, key, now)) else {
            return Cached::Miss;
        };
        if let Some(src) = &view.source {
            if !src.still_valid() {
                return Cached::Miss;
            }
        }
        let svg = match view.content {
            Content::Ok { svg, .. } => svg,
            Content::Failed(msg) => return Cached::Failed(msg),
        };
        if let Some((r, w, h)) = view.raster {
            return Cached::Hit(r, w, h);
        }
        match rasterize_svg(&svg) {
            Ok((buf, w, h)) => {
                let raster: Raster = Arc::new(buf);
                self.with(|s| s.attach_raster(instance, view.generation, (raster.clone(), w, h), now));
                Cached::Hit(raster, w, h)
            }
            Err(e) => {
                let msg = format!("トレース結果を描けない: {e:#}");
                self.with(|s| s.mark_failed(instance, view.generation, &msg));
                Cached::Failed(msg)
            }
        }
    }

    pub fn lend(&self, thread: u32, raster: Raster, now: Instant) -> *const u8 {
        self.with(|s| s.lend(thread, raster, now))
    }

    /// ファイル入力の画像を、読んだものがあればそれを、無ければ読んで返す（読むのは Mutex の外）
    pub fn decoded_image(&self, path: &Path, now: Instant) -> Result<(Arc<RgbaImage>, Option<SourceStamp>)> {
        let stamp = SourceStamp::of(path);
        if let Some(st) = &stamp {
            if let Some(img) = self.with(|s| s.decoded_get(st, now)) {
                return Ok((img, stamp));
            }
        }
        let img = Arc::new(trace::load_image_file(path)?);
        if let Some(st) = &stamp {
            self.with(|s| s.decoded_put(st.clone(), img.clone(), now));
        }
        Ok((img, stamp))
    }

    /// トレース結果（失敗も）をスロットに入れ、`export_dir` があれば（自動書き出しが ON）書き出す。
    ///
    /// 書けなかったものは `errors` に入れ、原因ごとに 1 回だけログに出す（描画は止めない）
    pub fn store_result(
        &self,
        result: std::result::Result<TraceResult, String>,
        cfg: &TraceConfig,
        source: Option<SourceStamp>,
        export_dir: Option<&Path>,
        now: Instant,
    ) -> StoreOutcome {
        // 連番を初めて使う書き出し先と画像名なら、フォルダの既存のファイルを Mutex の外で数える
        let scanned = match (export_dir, cfg.export_meta().export_name()) {
            (Some(dir), ExportName::Numbered(stem)) if !self.with(|s| s.knows_sequence(dir, &stem)) => {
                Some(trace::next_stem_sequence(dir, &stem))
            }
            _ => None,
        };
        let plan = self.with(|s| {
            let svg = s.insert_result(result, cfg, source, now)?;
            let dir = export_dir?;
            Some((svg.clone(), s.plan_export(cfg, &svg, dir, scanned, now)))
        });
        let mut outcome = StoreOutcome::default();
        let Some((svg, writes)) = plan else {
            return outcome;
        };
        for w in writes {
            let res = match &w.kind {
                WriteKind::Overwrite => write_file(&w.path, &svg).map(|()| w.path.clone()),
                WriteKind::NewNumbered { dir, stem } => self.write_new_numbered(&w.path, dir, stem, &svg),
            };
            match res {
                Ok(path) => {
                    if w.is_latest {
                        outcome.latest_written = true;
                    } else {
                        outcome.exported = Some(path);
                    }
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    self.with(|s| {
                        s.forget_written(&w.path);
                        s.warn_once("export", format!("SVG_TRACE: 自動書き出しに失敗した: {msg}"));
                    });
                    outcome.errors.push(msg);
                }
            }
        }
        outcome
    }

    /// 新しい連番のファイルを作る。同じ名前のファイルが既にあれば（ほかで作られた）、数え直して次の番号にする
    fn write_new_numbered(&self, path: &Path, dir: &Path, stem: &str, svg: &str) -> Result<PathBuf> {
        std::fs::create_dir_all(dir).with_context(|| format!("フォルダを作れない: {}", dir.display()))?;
        let mut path = path.to_path_buf();
        for _ in 0..16 {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    f.write_all(svg.as_bytes())
                        .with_context(|| format!("書けない: {}", path.display()))?;
                    return Ok(path);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let scanned = trace::next_stem_sequence(dir, stem);
                    path = self.with(|s| s.renumber(dir, stem, scanned));
                }
                Err(e) => return Err(e).with_context(|| format!("書けない: {}", path.display())),
            }
        }
        anyhow::bail!("連番のファイル名を決められない: {}", path.display())
    }

    /// インスタンス（無ければ直前のトレース結果）の SVG を `path` に書く。前に同じ内容を書いていれば書かない。
    /// 書いたら true
    pub fn write_svg(&self, path: &Path, instance: Option<&str>) -> Result<bool> {
        let svg = self
            .with(|s| s.svg_for(instance).map(|(svg, _, _)| svg))
            .ok_or_else(|| anyhow::anyhow!("write_latest: no svg"))?;
        let h = hash_str(&svg);
        if !self.with(|s| s.mark_written(path, h)) {
            return Ok(false);
        }
        if let Err(e) = write_file(path, &svg) {
            self.with(|s| s.forget_written(path));
            return Err(e);
        }
        Ok(true)
    }
}

fn write_file(path: &Path, svg: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("フォルダを作れない: {}", parent.display()))?;
    }
    std::fs::write(path, svg.as_bytes()).with_context(|| format!("書けない: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::{content_key, vectorize_rgba};
    use image::{Rgba, RgbaImage};

    fn temp_dir(name: &str) -> PathBuf {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("store_test")
            .join(name);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
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

    fn put(store: &SharedStore, r: TraceResult, c: &TraceConfig, dir: Option<&Path>, now: Instant) -> StoreOutcome {
        store.store_result(Ok(r), c, None, dir, now)
    }

    fn hit(store: &SharedStore, instance: &str, key: &str) -> Option<(Raster, u32, u32)> {
        store.render_cached(instance, key, Instant::now()).hit()
    }

    /// 自動書き出し OFF ではファイルを 1 つも書かない（v0.1.4 は受け渡しファイルを 2 本書いていた）
    #[test]
    fn auto_export_off_writes_nothing() {
        let root = temp_dir("off");
        let store = SharedStore::new();
        let now = Instant::now();
        for (i, color) in ["#ff0000", "#00ff00", "#0000ff"].iter().enumerate() {
            // OFF のとき lib.rs は書き出し先を渡さない
            let out = put(&store, rect_svg(color), &cfg("A", &format!("k{i}"), false), None, now);
            assert_eq!(out, StoreOutcome::default());
        }
        assert!(files_in(&root).is_empty(), "OFF なのにファイルができた: {:?}", files_in(&root));
        // aux2 はメモリから読む
        assert!(store.with(|s| s.svg_for(None)).unwrap().0.contains("#0000ff"));
    }

    /// 自動書き出し ON: 間が空けば次の番号、latest.svg も書く
    #[test]
    fn auto_export_on_writes_named_files() {
        let dir = temp_dir("on").join("SVG_export");
        let store = SharedStore::new();
        let t0 = Instant::now();
        let out = put(&store, rect_svg("#ff0000"), &cfg("A", "k0", true), Some(&dir), t0);
        assert_eq!(out.exported, Some(dir.join("logo_000.svg")));
        assert!(out.latest_written);
        let out = put(&store, rect_svg("#00ff00"), &cfg("A", "k1", true), Some(&dir), t0 + EXPORT_COALESCE * 2);
        assert_eq!(out.exported, Some(dir.join("logo_001.svg")));
        assert_eq!(files_in(&dir), ["latest.svg", "logo_000.svg", "logo_001.svg"]);
        assert!(std::fs::read_to_string(dir.join("latest.svg")).unwrap().contains("#00ff00"));
    }

    /// スライダーのドラッグ（続けてトレースし直す）では同じ番号を上書きし、番号を増やさない
    #[test]
    fn drag_overwrites_same_number() {
        let dir = temp_dir("drag").join("SVG_export");
        let store = SharedStore::new();
        let t0 = Instant::now();
        for (i, color) in ["#100000", "#200000", "#300000", "#400000", "#500000"].iter().enumerate() {
            let at = t0 + Duration::from_millis(300 * i as u64);
            let out = put(&store, rect_svg(color), &cfg("A", &format!("k{i}"), true), Some(&dir), at);
            assert_eq!(out.exported, Some(dir.join("logo_000.svg")), "{i} 段目");
        }
        assert_eq!(files_in(&dir), ["latest.svg", "logo_000.svg"]);
        assert!(std::fs::read_to_string(dir.join("logo_000.svg")).unwrap().contains("#500000"));
        // 別のオブジェクトなら続けていても別の番号
        let out = put(&store, rect_svg("#600000"), &cfg("B", "k", true), Some(&dir), t0 + Duration::from_millis(1500));
        assert_eq!(out.exported, Some(dir.join("logo_001.svg")));
    }

    /// 前に書いたものと同じ内容なら、新しい番号も latest.svg も書かない
    #[test]
    fn same_content_is_not_exported_again() {
        let dir = temp_dir("same").join("SVG_export");
        let store = SharedStore::new();
        let t0 = Instant::now();
        put(&store, rect_svg("#ff0000"), &cfg("A", "k0", true), Some(&dir), t0);
        let out = put(&store, rect_svg("#ff0000"), &cfg("A", "k1", true), Some(&dir), t0 + EXPORT_COALESCE * 2);
        assert_eq!(out, StoreOutcome::default());
        assert_eq!(files_in(&dir), ["latest.svg", "logo_000.svg"]);
    }

    /// 999 で頭打ちにしない（v0.1.4 は 999 の次も 999 で、黙って上書きしていた）。
    /// ほかで作られた同じ名前のファイルも上書きしない
    #[test]
    fn numbering_goes_past_999_and_never_overwrites() {
        let dir = temp_dir("seq").join("SVG_export");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("logo_999.svg"), "keep").unwrap();
        let store = SharedStore::new();
        let t0 = Instant::now();
        let out = put(&store, rect_svg("#ff0000"), &cfg("A", "k0", true), Some(&dir), t0);
        assert_eq!(out.exported, Some(dir.join("logo_1000.svg")));
        // 番号を覚えた後に、次の番号のファイルがほかで作られた
        std::fs::write(dir.join("logo_1001.svg"), "keep").unwrap();
        let out = put(&store, rect_svg("#00ff00"), &cfg("A", "k1", true), Some(&dir), t0 + EXPORT_COALESCE * 2);
        assert_eq!(out.exported, Some(dir.join("logo_1002.svg")));
        assert_eq!(std::fs::read_to_string(dir.join("logo_999.svg")).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(dir.join("logo_1001.svg")).unwrap(), "keep");
    }

    /// レイヤー入力の名前は上書き。同じ内容なら書かない
    #[test]
    fn layer_export_overwrites_fixed_name() {
        let dir = temp_dir("layer").join("SVG_export");
        let store = SharedStore::new();
        let layer_cfg = |key: &str| TraceConfig {
            export_stem: None,
            export_layer: Some(3),
            export_frame: Some(10),
            ..cfg("A", key, true)
        };
        let now = Instant::now();
        let out = put(&store, rect_svg("#ff0000"), &layer_cfg("k0"), Some(&dir), now);
        assert_eq!(out.exported, Some(dir.join("layer3_10.svg")));
        let out = put(&store, rect_svg("#ff0000"), &layer_cfg("k1"), Some(&dir), now);
        assert_eq!(out.exported, None);
    }

    /// 書けなかったら警告を 1 回だけ出し、結果はキャッシュに残る（描画は止めない）
    #[test]
    fn export_failure_keeps_result() {
        let root = temp_dir("fail");
        // フォルダのはずの場所にファイルを置いておく
        let dir = root.join("SVG_export");
        std::fs::write(&dir, "file").unwrap();
        let store = SharedStore::new();
        let now = Instant::now();
        let out = put(&store, rect_svg("#ff0000"), &cfg("A", "k", true), Some(&dir), now);
        assert!(!out.errors.is_empty());
        assert!(store.with(|s| s.warned("export")));
        assert!(hit(&store, "A", "k").is_some());
    }

    /// 2 インスタンスがそれぞれの結果を描く（v0.1.2 は後からトレースした方の SVG を両方に描いた）
    #[test]
    fn two_instances_do_not_mix() {
        let store = SharedStore::new();
        let now = Instant::now();
        put(&store, rect_svg("#ff0000"), &cfg("A", "file|a.png", false), None, now);
        put(&store, rect_svg("#0000ff"), &cfg("B", "file|b.png", false), None, now);
        // v0.1.2 の描き方（直前の結果）では A も青になる。検査が空振りしていないことの確認
        assert!(store.with(|s| s.svg_for(None)).unwrap().0.contains("#0000ff"));

        let (p, w, h) = hit(&store, "A", "file|a.png").expect("A hit");
        assert_eq!(center_pixel(&p, w, h), [255, 0, 0, 255], "A に B の結果が描かれた");
        let (p, w, h) = hit(&store, "B", "file|b.png").expect("B hit");
        assert_eq!(center_pixel(&p, w, h), [0, 0, 255, 255]);
        // 2 回目は描いた画素を使い回しても同じ
        let (p, w, h) = hit(&store, "A", "file|a.png").expect("A hit again");
        assert_eq!(center_pixel(&p, w, h), [255, 0, 0, 255]);

        // 入力のキーが違えば当てない。知らないインスタンスも当てない
        assert!(hit(&store, "A", "file|b.png").is_none());
        assert!(hit(&store, "C", "file|a.png").is_none());
        assert!(store.with(|s| s.svg_for(Some("A"))).unwrap().0.contains("#ff0000"));
    }

    /// 実際のトレース結果でも混ざらない（vectorize → store → render）
    #[test]
    fn two_instances_real_trace() {
        let store = SharedStore::new();
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
        let now = Instant::now();
        let a = vectorize_rgba(make([220, 30, 30, 255]).as_raw(), 32, 32, &color("A", "ka")).unwrap();
        let b = vectorize_rgba(make([30, 30, 220, 255]).as_raw(), 32, 32, &color("B", "kb")).unwrap();
        put(&store, a, &color("A", "ka"), None, now);
        put(&store, b, &color("B", "kb"), None, now);
        let (p, w, h) = hit(&store, "A", "ka").unwrap();
        let px = center_pixel(&p, w, h);
        assert!(px[0] > 180 && px[2] < 80, "A が赤でない: {px:?}");
        let (p, w, h) = hit(&store, "B", "kb").unwrap();
        let px = center_pixel(&p, w, h);
        assert!(px[2] > 180 && px[0] < 80, "B が青でない: {px:?}");
    }

    /// ファイル入力は元画像が書き換わったら当てない
    #[test]
    fn file_source_change_misses() {
        let root = temp_dir("stamp");
        let src = root.join("src.png");
        std::fs::write(&src, b"1234").unwrap();
        let store = SharedStore::new();
        store.store_result(Ok(rect_svg("#ff0000")), &cfg("A", "k", false), SourceStamp::of(&src), None, Instant::now());
        assert!(hit(&store, "A", "k").is_some());
        std::fs::write(&src, b"123456").unwrap();
        assert!(hit(&store, "A", "k").is_none());
    }

    /// 失敗も覚え、同じ入力では失敗を返す（トレースし直さない）。入力が変われば外れる
    #[test]
    fn failure_is_cached_per_key() {
        let store = SharedStore::new();
        let now = Instant::now();
        store.store_result(Err("細かすぎる".into()), &cfg("A", "k", false), None, None, now);
        match store.render_cached("A", "k", now) {
            Cached::Failed(m) => assert!(m.contains("細かすぎる")),
            other => panic!("失敗を覚えていない: {other:?}"),
        }
        assert!(matches!(store.render_cached("A", "k2", now), Cached::Miss));
        // aux2 へは送らない
        assert!(store.with(|s| s.svg_for(Some("A"))).is_none());
    }

    /// スロットは MAX_SLOTS を超えたら古いものから捨てる。直前まで使っていたものを捨てたときだけ警告する
    #[test]
    fn slots_are_bounded_and_thrash_is_warned() {
        let store = SharedStore::new();
        let long_ago = Instant::now();
        for i in 0..(MAX_SLOTS + 5) {
            put(&store, rect_svg("#ff0000"), &cfg(&format!("I{i}"), "k", false), None, long_ago + Duration::from_secs(10 * i as u64));
        }
        assert_eq!(store.with(|s| s.slot_count()), MAX_SLOTS);
        assert!(!store.with(|s| s.warned("slots")), "古いものを捨てただけで警告した");
        assert!(hit(&store, "I0", "k").is_none());
        assert!(hit(&store, &format!("I{}", MAX_SLOTS + 4), "k").is_some());

        // 33 個が同時に見えている（毎フレーム全部を使う）
        let store = SharedStore::new();
        let now = Instant::now();
        for i in 0..(MAX_SLOTS + 1) {
            put(&store, rect_svg("#ff0000"), &cfg(&format!("J{i}"), "k", false), None, now);
        }
        assert!(store.with(|s| s.warned("slots")));
    }

    /// バイト数の上限を超えたら、描いた画素から捨てる
    #[test]
    fn bytes_are_bounded() {
        let store = SharedStore::new();
        let now = Instant::now();
        // 2000x2000 の描いた画素は 16 MB。32 個描くと 512 MB で上限を超える
        let big = TraceResult {
            svg: r#"<svg xmlns="http://www.w3.org/2000/svg" width="2000" height="2000"><rect width="2000" height="2000" fill="red"/></svg>"#.into(),
            width: 2000,
            height: 2000,
        };
        let n = MAX_SLOTS;
        for i in 0..n {
            let inst = format!("B{i}");
            put(&store, big.clone(), &cfg(&inst, "k", false), None, now + Duration::from_secs(10 * i as u64));
            store.render_cached(&inst, "k", now + Duration::from_secs(10 * i as u64));
        }
        let total = store.with(|s| s.total_bytes());
        assert!(total <= MAX_BYTES, "上限を超えている: {total}");
        // スロットは残り、描いた画素だけ捨てられている
        assert_eq!(store.with(|s| s.slot_count()), n);
        assert!(!store.with(|s| s.has_raster("B0")));
        assert!(store.with(|s| s.has_raster(&format!("B{}", n - 1))));
    }

    /// しばらく使っていない描いた画素は捨てる（削除したオブジェクトの分が残り続けない）
    #[test]
    fn idle_raster_is_dropped() {
        let store = SharedStore::new();
        let t0 = Instant::now();
        put(&store, rect_svg("#ff0000"), &cfg("A", "k", false), None, t0);
        store.render_cached("A", "k", t0);
        assert!(store.with(|s| s.has_raster("A")));
        put(&store, rect_svg("#00ff00"), &cfg("B", "k", false), None, t0 + RASTER_IDLE + Duration::from_secs(1));
        assert!(!store.with(|s| s.has_raster("A")));
        // SVG は残っているので、トレースし直さずに描き直せる
        assert!(store.render_cached("A", "k", t0 + RASTER_IDLE * 2).hit().is_some());
    }

    /// Mutex の外で描いている間にスロットが入れ替わったら、古い画素を付けない
    #[test]
    fn stale_raster_is_not_attached() {
        let store = SharedStore::new();
        let now = Instant::now();
        put(&store, rect_svg("#ff0000"), &cfg("A", "k0", false), None, now);
        let generation = store.with(|s| s.lookup("A", "k0", now).unwrap().generation);
        put(&store, rect_svg("#0000ff"), &cfg("A", "k1", false), None, now);
        store.with(|s| s.attach_raster("A", generation, (Arc::new(vec![1, 2, 3, 4]), 1, 1), now));
        assert!(!store.with(|s| s.has_raster("A")));
        let (p, w, h) = hit(&store, "A", "k1").unwrap();
        assert_eq!(center_pixel(&p, w, h), [0, 0, 255, 255]);
    }

    /// Lua へ貸した画素は、スロットが入れ替わっても捨てられても、同じスレッドが次に
    /// 受け取るまで解放されない（v0.1.3 はスロットを捨てた時点で解放していた）
    #[test]
    fn lent_raster_outlives_slot_replacement() {
        let store = SharedStore::new();
        let t0 = Instant::now();
        put(&store, rect_svg("#ff0000"), &cfg("A", "k0", false), None, t0);
        let (raster, w, h) = hit(&store, "A", "k0").unwrap();
        let weak = Arc::downgrade(&raster);
        let ptr = store.lend(1, raster, t0);

        // 別のオブジェクトの処理に見立てて、A のスロットを入れ替え、さらに追い出す
        put(&store, rect_svg("#0000ff"), &cfg("A", "k1", false), None, t0);
        for i in 0..(MAX_SLOTS + 2) {
            put(&store, rect_svg("#00ff00"), &cfg(&format!("X{i}"), "k", false), None, t0);
        }
        assert!(hit(&store, "A", "k1").is_none(), "A は追い出されているはず");
        assert!(weak.upgrade().is_some(), "貸した画素が解放された");
        let lent = unsafe { std::slice::from_raw_parts(ptr, (w * h * 4) as usize) };
        assert_eq!(center_pixel(lent, w, h), [255, 0, 0, 255]);

        // 同じスレッドが次の画素を受け取ったら、前の貸し出しは外れる
        put(&store, rect_svg("#0000ff"), &cfg("A", "k1", false), None, t0);
        let (next, _, _) = hit(&store, "A", "k1").unwrap();
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
        let store = Arc::new(SharedStore::new());
        let colors = [("#ff0000", [255, 0, 0, 255]), ("#00ff00", [0, 255, 0, 255]), ("#0000ff", [0, 0, 255, 255])];
        let handles: Vec<_> = colors
            .iter()
            .enumerate()
            .map(|(n, (color, expect))| {
                let store = Arc::clone(&store);
                let color = color.to_string();
                let expect = *expect;
                std::thread::spawn(move || {
                    let thread = n as u32 + 1;
                    for i in 0..200 {
                        let inst = format!("T{n}");
                        let key = format!("k{i}");
                        let now = Instant::now();
                        put(&store, rect_svg(&color), &cfg(&inst, &key, false), None, now);
                        // ほかのスレッドのスロットを追い出す
                        for j in 0..(MAX_SLOTS / 2) {
                            put(&store, rect_svg("#ffffff"), &cfg(&format!("F{n}_{j}"), "k", false), None, now);
                        }
                        let Some((r, w, h)) = store.render_cached(&inst, &key, now).hit() else {
                            // 追い出しの競争で外れたら、トレースし直すのと同じ扱いで次へ
                            continue;
                        };
                        let ptr = store.lend(thread, r, now);
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

    /// aux2 の「SVG_Hへ送る」は SDK のオブジェクト ID で引く
    #[test]
    fn svg_for_object_matches_object_id() {
        let store = SharedStore::new();
        let now = Instant::now();
        put(&store, rect_svg("#ff0000"), &cfg("12345", "k", false), None, now);
        put(&store, rect_svg("#0000ff"), &cfg("1e+15", "k", false), None, now);
        assert!(store.with(|s| s.svg_for_object(12345)).unwrap().contains("#ff0000"));
        assert!(store.with(|s| s.svg_for_object(1_000_000_000_000_000)).unwrap().contains("#0000ff"));
        assert!(store.with(|s| s.svg_for_object(999)).is_none());
        assert!(store.with(|s| s.svg_for_object(0)).is_none());
    }

    /// 同じ内容なら「出力SVG」も書き直さない
    #[test]
    fn write_svg_skips_same_content() {
        let root = temp_dir("write");
        let store = SharedStore::new();
        put(&store, rect_svg("#ff0000"), &cfg("A", "k", false), None, Instant::now());
        let p = root.join("out.svg");
        assert!(store.write_svg(&p, Some("A")).unwrap());
        assert!(!store.write_svg(&p, Some("A")).unwrap());
        put(&store, rect_svg("#00ff00"), &cfg("A", "k2", false), None, Instant::now());
        assert!(store.write_svg(&p, Some("A")).unwrap());
        assert!(std::fs::read_to_string(&p).unwrap().contains("#00ff00"));
    }

    /// 読んだ画像は、同じファイルなら使い回し、書き換わったら読み直す
    #[test]
    fn decoded_image_is_reused() {
        let root = temp_dir("decoded");
        let path = root.join("a.png");
        RgbaImage::from_pixel(8, 8, Rgba([1, 2, 3, 255])).save(&path).unwrap();
        let store = SharedStore::new();
        let now = Instant::now();
        let (a, _) = store.decoded_image(&path, now).unwrap();
        let (b, _) = store.decoded_image(&path, now).unwrap();
        assert!(Arc::ptr_eq(&a, &b), "同じファイルを読み直した");
        std::thread::sleep(Duration::from_millis(20));
        RgbaImage::from_pixel(9, 8, Rgba([1, 2, 3, 255])).save(&path).unwrap();
        let (c, _) = store.decoded_image(&path, now).unwrap();
        assert_eq!(c.width(), 9);
        assert_eq!(store.with(|s| s.decoded_count()), 1);
        // しばらく使わなければ捨てる
        put(&store, rect_svg("#ff0000"), &cfg("A", "k", false), None, now + RASTER_IDLE * 2);
        assert_eq!(store.with(|s| s.decoded_count()), 0);
    }

    /// レイヤー入力の模擬: 下が静止画なら 300 フレームでトレースは 1 回。同じフレームで編集したらトレースし直す
    /// （obj2 v0.1.3 はキーにフレーム番号を入れていて 300 回トレースし、編集しても描き直さなかった）
    #[test]
    fn layer_input_traces_once_for_still_image() {
        let store = SharedStore::new();
        let (w, h) = (48u32, 48u32);
        let mut img = RgbaImage::from_pixel(w, h, Rgba([255, 255, 255, 255]));
        for y in 10..38 {
            for x in 10..38 {
                img.put_pixel(x, y, Rgba([0, 0, 0, 255]));
            }
        }
        let mut traces = 0;
        let frame_of = |pixels: &[u8], traces: &mut i32| {
            // SVG_TRACE.obj2 v0.2.0 のキー: レイヤー番号と画素の中身
            let key = format!("layer:2:{}|0|8", content_key(pixels, w, h));
            let now = Instant::now();
            if store.render_cached("1", &key, now).hit().is_some() {
                return;
            }
            let c = cfg("1", &key, false);
            let r = vectorize_rgba(pixels, w, h, &c).unwrap();
            store.store_result(Ok(r), &c, None, None, now);
            assert!(store.render_cached("1", &key, now).hit().is_some());
            *traces += 1;
        };
        for _ in 0..300 {
            frame_of(img.as_raw(), &mut traces);
        }
        assert_eq!(traces, 1, "静止画なのにトレースし直した");
        // 同じフレームで下のレイヤーを編集した（1 画素だけ変えた）
        img.put_pixel(0, 0, Rgba([0, 0, 0, 255]));
        frame_of(img.as_raw(), &mut traces);
        assert_eq!(traces, 2, "編集したのに描き直さない");
    }
}
