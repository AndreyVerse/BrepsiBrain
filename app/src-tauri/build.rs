fn main() {
    // Панель грузится с http://127.0.0.1:8765, а для удалённого origin Tauri пускает
    // команды приложения только по явному allow-<команда> в capability: без манифеста
    // обновление и выбор папки отвергались «not allowed by ACL».
    let attributes = tauri_build::Attributes::new()
        .app_manifest(tauri_build::AppManifest::new().commands(&["pick_folder", "check_update", "install_update"]));
    tauri_build::try_build(attributes).expect("failed to run tauri-build");
}
