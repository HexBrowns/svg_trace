# svg_trace

AviUtl2 向けビットマップ→SVG ベクター化プラグイン。

- **バージョン:** 0.1.3
- **作者:** HexBrowns

## 機能

- 二値 / カラー（2〜16色）トレース（vtracer）
- 入力: ファイル / レイヤー（絶対・相対）
- プレビュー表示と SVG 自動書き出し（「自動書き出し」ON のときだけ。v0.1.3 から）
  - プロジェクト保存済み: `{プロジェクトフォルダ}/SVG_export/`
  - 未保存: `Plugin/svg_trace/export/`
  - ファイル入力: `{画像名}_{000連番}.svg` / レイヤー入力: `layer{番号}_{フレーム}.svg`
- 「SVG_Hへ送る」用に、トレースのたびに `Plugin/svg_trace/latest_trace.svg` を 1 本だけ上書きする
- トレース結果のキャッシュはオブジェクトごと（2 つ以上置いても結果が混ざらない。v0.1.3 から）
- オブジェクトメニュー「SVG_TRACE → SVG_Hへ送る」で `SVG_H` 生成

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

ビルドと同梱物は `aviutl2.toml`（[aviutl2-cli](https://github.com/sevenc-nanashi/aviutl2-cli)）が正本。このフォルダで `au2 release` だけを実行すると `release/` に au2pkg ができる。

## 使い方

1. タイムラインに「SVG_TRACE」オブジェクトを追加
2. 入力をファイルまたはレイヤーに設定してトレース
3. 線編集する場合はオブジェクト右クリック →「SVG_TRACE → SVG_Hへ送る」
