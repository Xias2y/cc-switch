# Windows NSIS installer

`installer.nsi` is based on Tauri CLI 2.10.1's template:
https://github.com/tauri-apps/tauri/blob/tauri-cli-v2.10.1/crates/tauri-bundler/src/bundle/windows/nsis/installer.nsi

It retains Tauri's normal install/update/uninstall behavior. Local changes share MSI detection between installation-directory restoration and migration, including HKCU and HKLM installations. Existing NSIS registration takes precedence; an explicit `/D=` directory keeps precedence because restoration is called only for the default placeholder. MSI `InstallLocation` is captured before uninstalling the old product. Match the product name, publisher and MSI uninstall command before inheriting a directory or uninstalling a product.

When updating the Tauri CLI, compare this file against its bundled template before rebuilding. The upstream template is available under the MIT license in `LICENSE-MIT`.
