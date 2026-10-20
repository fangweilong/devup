//! Windows 可执行文件资源编译：把图标与版本信息写入 PE 的 `.rsrc` 段。
//!
//! 说明：`tao` 的 `with_window_icon` 只设置**运行期窗口图标**，
//! 资源管理器中 exe 文件本身显示的图标来自 PE 资源段，二者互不相通。
//! 没有这一步，打包出的 devup.exe 在资源管理器里就是系统默认的空白图标。

fn main() {
    #[cfg(windows)]
    windows::compile();
}

#[cfg(windows)]
mod windows {
    use std::fs;
    use std::path::PathBuf;

    pub fn compile() {
        let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let ico = manifest_dir.join("packaging").join("windows").join("devup.ico");
        // 图标与本脚本任一变化都触发重新编译资源
        println!("cargo:rerun-if-changed={}", ico.display());
        println!("cargo:rerun-if-changed=build.rs");

        if !ico.exists() {
            println!(
                "cargo:warning=未找到 {}，跳过 Windows 图标资源编译",
                ico.display()
            );
            return;
        }

        let rc = manifest_dir.join("target").join("windows_resource.rc");
        let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
        // 版本号需为 x.y.z.w 形式，缺失位补 0
        let mut parts: Vec<u32> = version
            .split('-')
            .next()
            .unwrap_or("0.0.0")
            .split('.')
            .filter_map(|p| p.parse().ok())
            .collect();
        while parts.len() < 4 {
            parts.push(0);
        }
        let comma_ver = parts
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(", ");

        let rc_body = format!(
            r#"1 VERSIONINFO
FILEVERSION {comma}
PRODUCTVERSION {comma}
FILEOS 0x40004L
FILETYPE 0x1L
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "080404b0"
    BEGIN
      VALUE "FileDescription", "DevUp - 现代化开发环境与 AI 工具链管家\0"
      VALUE "FileVersion", "{version}\0"
      VALUE "ProductName", "DevUp\0"
      VALUE "ProductVersion", "{version}\0"
      VALUE "OriginalFilename", "devup.exe\0"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x804, 1200
  END
END

1 ICON "{ico_path}"
"#,
            comma = comma_ver,
            version = version,
            ico_path = ico.to_string_lossy().replace('\\', "/"),
        );

        if let Err(e) = fs::write(&rc, rc_body) {
            println!("cargo:warning=写入资源脚本失败: {e}");
            return;
        }

        // 3.x 返回 CompilationResult 枚举：仅在真正失败时报错，
        // 未安装资源编译器(如纯 Linux 构建)不应阻断编译。
        match embed_resource::compile(&rc, embed_resource::NONE) {
            embed_resource::CompilationResult::Failed(msg) => {
                panic!("编译 Windows 资源(图标/版本信息)失败: {msg}");
            }
            embed_resource::CompilationResult::NotAttempted(msg) => {
                println!("cargo:warning=未能编译 Windows 资源，exe 将不含图标: {msg}");
            }
            _ => {}
        }
    }
}