# svg_trace

AviUtl2 向けビットマップ→SVG ベクター化プラグイン。

- **バージョン:** 0.2.0
- **作者:** HexBrowns

## 機能

- 二値 / カラー（2〜16色）トレース（vtracer）
- 入力: ファイル / レイヤー（絶対・相対）
- プレビュー表示と SVG 自動書き出し（「自動書き出し」ON のときだけ。OFF ならファイルを書かない）
  - プロジェクト保存済み: `{プロジェクトフォルダ}/SVG_export/`
  - 未保存: `Plugin/svg_trace/export/`
  - ファイル入力: `{画像名}_{000連番}.svg`（ドラッグ中は同じ番号を上書き。999 の次は 1000） / レイヤー入力: `layer{番号}_{フレーム}.svg`
- トレース結果のキャッシュはオブジェクトごと（2 つ以上置いても結果が混ざらない）。レイヤー入力は画素の中身（xxh3）で引くので、
  下が静止画なら 1 回で済み、編集したら描き直す。失敗も覚え、同じ入力で毎フレームトレースし直さない
- キャッシュは 32 個・256 MB まで。1 分使われていない描いた画素と読んだ画像は捨てる
- オブジェクトメニュー「SVG_TRACE → SVG_Hへ送る」で `SVG_H` 生成。aux2 は mod2 の `svg_trace_copy_svg_v1`（同じプロセスに
  読み込まれた `svg_trace.mod2` を `GetModuleHandleExW` で探す）でメモリの結果を読む。v0.1.4 までの受け渡しファイル
  （`Plugin/svg_trace/latest_trace.svg` と `traces/`）は書かず、起動時に片付ける
- ファイルの読み書きと SVG の描画は Mutex の外で行う（`mod2/src/store.rs` の先頭）

## 配置

| パス | 内容 |
|------|------|
| `Script/svg_trace/svg_trace.mod2` | トレースコア |
| `Script/svg_trace/SVG_TRACE.obj2` | カスタムオブジェクト |
| `Plugin/svg_trace/svg_trace.aux2` | SVG_H 連携メニュー |

## ビルド

```powershell
python AI/tools/au2_build.py svg_trace               # au2 release → 本番（C:\ProgramData\aviutl2）へ配置
python AI/tools/au2_build.py svg_trace --no-deploy   # 配置しない（本番との違いだけ出す）
```

机上の測定（`#[ignore]`。時間は機械で変わる）:

```powershell
cargo test --release -p svg-trace-mod2 bench -- --ignored --nocapture --test-threads=1
cargo build --release -p svg-trace-mod2; cargo test --release -p svg-trace-aux2 -- --ignored   # 本体と同じ名前で mod2 を読み込めるか
```

ビルドと同梱物は `aviutl2.toml`（[aviutl2-cli](https://github.com/sevenc-nanashi/aviutl2-cli)）が正本。このフォルダで `au2 release` だけを実行すると `release/` に au2pkg ができる。

## 使い方

1. タイムラインに「SVG_TRACE」オブジェクトを追加
2. 入力をファイルまたはレイヤーに設定してトレース
3. 線編集する場合はオブジェクト右クリック →「SVG_TRACE → SVG_Hへ送る」
