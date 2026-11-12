---
name: package-macos
description: 在本機把 flowsurface 打包成 macOS 的 .app 與 DMG。使用者說「打包」、「出 DMG」、「package」、「build app」，或要把目前的版本裝進 /Applications 時使用。預設只打包 Apple Silicon（aarch64）；只有使用者明確要 Intel 或 universal 時才打包 universal。
---

# 打包 macOS 版

這個 fork 只在 macOS 自用，所以本機打包預設用 **Apple Silicon（aarch64）**。universal 要編兩個架構，時間約加倍，只有使用者明確要求時才用。

## 1. 前置檢查

依序執行，任一項失敗就停下來回報，不要繼續打包：

```bash
git status --short                      # 有未 commit 的變更要先告知使用者：打包會包含它們
uname -m                                # 應為 arm64
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings 2>&1 | grep -E '^(error|warning)' | head -20
```

如果使用者只說「打包」、沒有要求先跑測試，以上檢查就夠了。要求完整驗證時，再加跑 CLAUDE.md「Rust Test」一節的測試。

## 2. 編譯與打包

編譯使用 `packaging` profile（fat LTO、`codegen-units = 1`），乾淨建置要好幾分鐘，請用背景執行，並在指令中過濾輸出：

```bash
# Apple Silicon（預設）
bash scripts/build-macos.sh aarch64 && bash scripts/package-macos.sh aarch64

# universal（arm64 + x86_64，只在使用者要求時）
bash scripts/build-macos.sh && bash scripts/package-macos.sh
```

- 兩支腳本的架構參數必須一致。`package-macos.sh` 會用 `lipo -verify_arch` 檢查 `target/packaging/flowsurface`，架構不符就停止。
- `build-macos.sh` 會執行 `rustup target add` 加入兩個 target；第一次可能需要下載。
- 在 `.claude/worktrees/<name>` 內打包時，可設 `CARGO_TARGET_DIR` 指向主 checkout 的 `target/`，以重用已編譯的依賴；但產物路徑也會跟著變到那裡。

## 3. 產物

| 模式 | DMG | .app |
|---|---|---|
| aarch64 | `target/packaging/flowsurface-fork-<upstream>_<fork>-aarch64-macos.dmg` | `target/packaging/macos/Flowsurface Fork.app` |
| universal | `target/packaging/flowsurface-fork-<upstream>_<fork>-universal-macos.dmg` | 同上 |

`build-macos.sh aarch64` 另外會產出 `target/packaging/flowsurface-aarch64-macos.tar.gz`，裡面只有執行檔。

## 4. 驗證後回報

```bash
lipo -archs "target/packaging/macos/Flowsurface Fork.app/Contents/MacOS/flowsurface"
codesign --verify --strict "target/packaging/macos/Flowsurface Fork.app" && echo SIGNATURE_OK
ls -lh target/packaging/*.dmg
```

回報時附上 DMG 的完整路徑、大小、架構與版本號 `<upstream>_<fork>`（upstream 取自 `Cargo.toml` 的 `workspace.package.version`，fork 取自 `src/version.rs` 的 `FORK_VERSION`）。

這個 fork 和 upstream 完全分開：app 名稱是「Flowsurface Fork」，bundle ID 是 `io.github.darkstar1227.flowsurface-fork`，資料目錄是 `flowsurface-fork/`，Keychain service 是 `flowsurface-fork.*`。因此可以和 upstream 的 `Flowsurface.app` 並存在 `/Applications`，安裝時不要覆蓋 upstream 的 app。

## 注意事項（要轉告使用者）

- 簽名是 ad-hoc（`codesign --sign -`），沒有 Apple Developer ID，也沒有 notarize。第一次開啟要按右鍵選「打開」，或到「系統設定 → 隱私權與安全性」允許。
- ad-hoc 簽名每次重新編譯都會變，Keychain 中的 proxy、server 憑證（以及日後的交易所金鑰）在換版後會重新詢問授權，選「永遠允許」即可。
- 不要用 `.github/workflows/release.yaml` 發版：它需要 upstream repo secrets 裡的 GPG 金鑰，fork 上無法執行。
- 不要自動安裝到 `/Applications`，也不要覆蓋使用者現有的 app，除非使用者明確要求。
