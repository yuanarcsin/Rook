//! 构建脚本：Tauri 资源（图标/版本/主清单）+ 测试目标的 Common Controls v6 清单。
//!
//! `cargo test` 的集成测试（`tests/*.rs`）会链接本 crate 的 lib，而 lib 在**非**
//! `cfg(test)` 构建里仍包含 Tauri 外壳（`engine`/`tray`/`ui`）。托盘代码静态导入
//! `comctl32!TaskDialogIndirect`，只有带 Common Controls v6 清单的进程才能解析；
//! 而 `tauri_build::build()` 生成的清单只链接到 bin 目标，集成测试 exe 没有清单，
//! 会以 `STATUS_ENTRYPOINT_NOT_FOUND`（0xC0000139）直接退出。
//!
//! 因此这里额外把一份最小清单链接到 **test 目标**（`rustc-link-arg-tests`），
//! 不影响产品二进制；`tauri_build` 仍负责 bin 的完整清单。

use std::path::PathBuf;

fn main() {
    tauri_build::build();

    #[cfg(windows)]
    link_test_manifest();
}

/// 给测试目标嵌入 Common Controls v6 清单（仅 Windows）。
#[cfg(windows)]
fn link_test_manifest() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let manifest = out_dir.join("cs-test.manifest");
    let rc = out_dir.join("cs-test.rc");

    // 测试进程只需要 Common Controls v6 依赖；不重复产品清单里的 DPI/权限设置。
    let manifest_body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency>
    <dependentAssembly>
      <assemblyIdentity
        type="win32"
        name="Microsoft.Windows.Common-Controls"
        version="6.0.0.0"
        processorArchitecture="*"
        publicKeyToken="6595b64144ccf1df"
        language="*" />
    </dependentAssembly>
  </dependency>
</assembly>
"#;

    // 资源 ID 1 = RT_MANIFEST。
    let rc_body = "1 24 \"cs-test.manifest\"\n";

    if std::fs::write(&manifest, manifest_body).is_err() || std::fs::write(&rc, rc_body).is_err() {
        println!("cargo:warning=写入测试清单失败，集成测试可能无法启动");
        return;
    }

    // 编译失败不阻断构建：产品二进制仍可正常构建，只是集成测试可能无法启动。
    match embed_resource::compile_for_tests(&rc, embed_resource::NONE) {
        embed_resource::CompilationResult::Ok => {}
        other => println!("cargo:warning=测试清单未链接：{other:?}"),
    }
}
