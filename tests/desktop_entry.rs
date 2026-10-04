//! Integração com o desktop: arquivo `.desktop`, ícone e instalador.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const APP_ID: &str = "dev.catchback.Catchback";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn desktop_file() -> PathBuf {
    root().join("data").join(format!("{APP_ID}.desktop"))
}

fn icon_file() -> PathBuf {
    root().join("data/icons/hicolor/scalable/apps").join(format!("{APP_ID}.svg"))
}

/// Chaves da seção `[Desktop Entry]`.
fn entries(path: &Path) -> HashMap<String, String> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut in_section = false;
    let mut map = HashMap::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_section = line == "[Desktop Entry]";
        } else if in_section && let Some((k, v)) = line.split_once('=') {
            map.insert(k.to_string(), v.to_string());
        }
    }
    map
}

fn tool_exists(name: &str) -> bool {
    Command::new(name).arg("--version").output().is_ok()
}

#[test]
fn app_id_in_the_code_matches_the_desktop_file_name() {
    // No Wayland o ícone e o agrupamento das janelas dependem de o nome do .desktop
    // ser igual ao ID do aplicativo.
    let main = std::fs::read_to_string(root().join("src/main.rs")).unwrap();
    assert!(main.contains(&format!("const APP_ID: &str = \"{APP_ID}\";")));
    assert!(desktop_file().exists(), "{}", desktop_file().display());
}

#[test]
fn desktop_entry_has_the_required_fields() {
    let e = entries(&desktop_file());
    assert_eq!(e["Type"], "Application");
    assert_eq!(e["Name"], "Catchback");
    assert_eq!(e["Icon"], APP_ID);
    assert_eq!(e["Exec"], "catchback");
    assert_eq!(e["Terminal"], "false");
    assert_eq!(e["StartupWMClass"], APP_ID);
    assert!(!e["Comment"].is_empty());
    assert!(e["Categories"].ends_with(';'), "Categories precisa terminar com ';'");
    for category in ["AudioVideo", "Recorder", "GTK"] {
        assert!(e["Categories"].split(';').any(|c| c == category), "falta {category} em {}", e["Categories"]);
    }
    assert!(e["Keywords"].split(';').any(|k| k == "replay"), "{}", e["Keywords"]);
}

#[test]
fn desktop_entry_passes_the_official_validator() {
    if !tool_exists("desktop-file-validate") {
        eprintln!("desktop-file-validate ausente: teste ignorado");
        return;
    }
    let out = Command::new("desktop-file-validate").arg(desktop_file()).output().unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn icon_is_a_scalable_svg_with_a_square_viewbox() {
    let svg = std::fs::read_to_string(icon_file()).unwrap_or_else(|e| panic!("{}: {e}", icon_file().display()));
    assert!(svg.contains("<svg") && svg.contains("xmlns=\"http://www.w3.org/2000/svg\""));
    assert!(svg.contains("viewBox=\"0 0 128 128\""), "o ícone deve ser quadrado (128x128)");
    assert!(svg.len() < 20_000, "ícone grande demais: {} bytes", svg.len());
}

#[test]
fn icon_renders_to_a_png_when_rsvg_is_available() {
    if !tool_exists("rsvg-convert") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    for size in [16, 64, 256] {
        let out = dir.path().join(format!("{size}.png"));
        let status = Command::new("rsvg-convert")
            .args(["-w", &size.to_string(), "-h", &size.to_string()])
            .arg(icon_file())
            .arg("-o")
            .arg(&out)
            .status()
            .unwrap();
        assert!(status.success(), "falha ao renderizar em {size}px");
        assert!(std::fs::metadata(&out).unwrap().len() > 100);
    }
}

fn install_script() -> Command {
    let mut cmd = Command::new("bash");
    cmd.arg(root().join("install.sh")).current_dir(root());
    cmd
}

#[test]
fn installer_copies_binary_entry_and_icon_then_uninstalls_cleanly() {
    let prefix = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_catchback");

    let out = install_script().args(["--prefix"]).arg(prefix.path()).args(["--bin", bin]).output().unwrap();
    assert!(out.status.success(), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));

    let installed_bin = prefix.path().join("bin/catchback");
    let installed_desktop = prefix.path().join(format!("share/applications/{APP_ID}.desktop"));
    let installed_icon = prefix.path().join(format!("share/icons/hicolor/scalable/apps/{APP_ID}.svg"));
    assert!(installed_bin.exists() && installed_desktop.exists() && installed_icon.exists());

    // O Exec do instalado aponta para o binário instalado (o PATH pode não incluir ~/.local/bin).
    let e = entries(&installed_desktop);
    assert_eq!(e["Exec"], installed_bin.to_string_lossy());
    assert_eq!(e["Icon"], APP_ID);
    if tool_exists("desktop-file-validate") {
        let v = Command::new("desktop-file-validate").arg(&installed_desktop).output().unwrap();
        assert!(v.status.success(), "{}", String::from_utf8_lossy(&v.stdout));
    }

    let out = install_script().args(["--uninstall", "--prefix"]).arg(prefix.path()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!installed_bin.exists() && !installed_desktop.exists() && !installed_icon.exists());
}

#[test]
fn installer_rejects_an_unknown_option() {
    let out = install_script().arg("--nao-existe").output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).to_lowercase().contains("opção"));
}

#[test]
fn installer_fails_clearly_when_the_given_binary_is_missing() {
    let prefix = tempfile::tempdir().unwrap();
    let out = install_script()
        .args(["--prefix"])
        .arg(prefix.path())
        .args(["--bin", "/caminho/que/nao/existe"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!prefix.path().join("bin").exists(), "não deve instalar nada se o binário não existe");
}
