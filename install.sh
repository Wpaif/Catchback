#!/usr/bin/env bash
# Instala o Catchback: binário, atalho do menu (.desktop) e ícone.
set -euo pipefail

APP_ID="dev.catchback.Catchback"
PREFIX="${HOME}/.local"
BIN=""
UNINSTALL=0
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

usage() {
    cat <<USO
Uso: ./install.sh [--prefix DIR] [--bin CAMINHO] [--uninstall]

  --prefix DIR    onde instalar (padrão: \$HOME/.local)
  --bin CAMINHO   usa um binário já compilado em vez de rodar 'cargo build --release'
  --uninstall     remove o que foi instalado
USO
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --prefix) PREFIX="${2:?--prefix precisa de um diretório}"; shift 2 ;;
        --bin) BIN="${2:?--bin precisa de um caminho}"; shift 2 ;;
        --uninstall) UNINSTALL=1; shift ;;
        -h | --help) usage; exit 0 ;;
        *) echo "Opção desconhecida: $1" >&2; usage >&2; exit 2 ;;
    esac
done

BIN_DEST="$PREFIX/bin/catchback"
DESKTOP_DEST="$PREFIX/share/applications/$APP_ID.desktop"
ICON_DEST="$PREFIX/share/icons/hicolor/scalable/apps/$APP_ID.svg"

refresh_caches() {
    update-desktop-database "$PREFIX/share/applications" 2>/dev/null || true
    gtk-update-icon-cache -q -t -f "$PREFIX/share/icons/hicolor" 2>/dev/null || true
}

if [[ $UNINSTALL -eq 1 ]]; then
    rm -f "$BIN_DEST" "$DESKTOP_DEST" "$ICON_DEST"
    refresh_caches
    echo "Catchback removido de $PREFIX"
    exit 0
fi

if [[ -z "$BIN" ]]; then
    echo "Compilando (release)…"
    (cd "$HERE" && cargo build --release)
    BIN="$HERE/target/release/catchback"
fi
if [[ ! -x "$BIN" ]]; then
    echo "Binário não encontrado: $BIN" >&2
    exit 1
fi

install -Dm755 "$BIN" "$BIN_DEST"
# O Exec aponta para o binário instalado: o PATH do menu pode não incluir ~/.local/bin.
mkdir -p "$(dirname "$DESKTOP_DEST")"
sed "s|^Exec=.*|Exec=$BIN_DEST|" "$HERE/data/$APP_ID.desktop" > "$DESKTOP_DEST"
chmod 644 "$DESKTOP_DEST"
install -Dm644 "$HERE/data/icons/hicolor/scalable/apps/$APP_ID.svg" "$ICON_DEST"
refresh_caches

echo "Catchback instalado em $PREFIX"
echo "  binário: $BIN_DEST"
echo "  atalho:  $DESKTOP_DEST"
echo "  ícone:   $ICON_DEST"
echo "Abra pelo menu de aplicativos ou rode: $BIN_DEST"
