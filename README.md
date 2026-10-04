# Catchback

Replay buffer e gravador de tela para Linux, feito para registrar aquele momento raro (um shiny, um drop, uma captura) **depois** que ele acontece, sem precisar lembrar de apertar "gravar" antes.

Interface em GTK4 + libadwaita, captura via PipeWire/xdg-desktop-portal e codificação com GStreamer.

## Modos

- **Replay**: mantém sempre os últimos N minutos (padrão: 10) da tela ou de uma janela. Ao ver algo digno de clip, clique em **Salvar últimos 2 min** e o trecho é salvo sem reencodar.
- **Gravação**: grava um único arquivo até você parar, como a gravação de tela do GNOME.

## Formatos e qualidade

| Formato | Codec  | Observação                                   |
|---------|--------|----------------------------------------------|
| MP4     | H.264  | Padrão. Aceito em Discord, navegadores, redes |
| MKV     | H.264  | Mais robusto contra travamentos               |
| WebM    | VP9    | Mais pesado para codificar                    |

| Qualidade           | Bitrate |
|---------------------|---------|
| Leve                | 8 Mbps  |
| **Alta** (padrão)   | 20 Mbps |
| Máxima              | 50 Mbps |

> A codificação é feita em software (x264/VP9). Em 1080p60 a qualidade Máxima e o VP9 exigem bastante da CPU; se o vídeo falhar, use Alta ou Leve.

## Requisitos

- Linux com **Wayland** ou X11 e `xdg-desktop-portal` (testado no GNOME/Wayland)
- Rust (edição 2024), via [rustup](https://rustup.rs)
- GTK 4 e libadwaita (com cabeçalhos de desenvolvimento)
- GStreamer com os plugins `base`, `good`, `bad` e `ugly`, mais o plugin do PipeWire
- `ffmpeg` (usado para juntar os segmentos do replay)

No Arch/Manjaro:

```sh
sudo pacman -S --needed rust gtk4 libadwaita ffmpeg xdg-desktop-portal \
    gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-plugins-ugly \
    gst-plugin-pipewire
```

## Como usar

```sh
cargo run --release
```

1. Escolha **Replay** ou **Gravação**.
2. Clique em iniciar e escolha a tela ou a janela no diálogo do sistema.
3. No replay, clique em **Salvar últimos X min** quando quiser guardar o momento. Na gravação, clique em **Parar**.

Os clips vão para `~/Vídeos/Catchback` (a pasta de vídeos do seu usuário). Tudo pode ser ajustado em **Preferências** (`Ctrl+,`): duração do buffer, duração do clip, FPS, qualidade, formato e pasta de saída.

A configuração fica em `~/.config/catchback/config.toml`:

```toml
buffer_minutes = 10   # janela do replay (1 a 60)
clip_seconds = 120    # quanto o botão "salvar" recupera (5 a 3600)
fps = 60
container = "mp4"     # mp4 | mkv | webm
quality = "high"      # light | high | max
output_dir = "/home/usuario/Vídeos/Catchback"
```

## Como funciona

No replay, o GStreamer grava segmentos de 5 s em `~/.cache/catchback/segments` (em disco, não em RAM). Um buffer circular descarta os mais antigos além da janela. Ao salvar, o segmento atual é fechado e os últimos segmentos são juntados com `ffmpeg -c copy`, por isso o clip sai quase instantaneamente e sem perda de qualidade.

Como no Wayland a fonte só entrega quadros quando a tela muda, o último quadro é reenviado periodicamente (`keepalive-time`), para que uma tela parada também gere vídeo.

## Estrutura

O núcleo é independente da interface e testável:

| Módulo        | Responsabilidade                                               |
|---------------|----------------------------------------------------------------|
| `buffer`      | Buffer circular de segmentos e seleção dos últimos N segundos  |
| `config`      | Configuração em TOML, validação e caminhos XDG                 |
| `session`     | Máquina de estados (parado, gravando, replay)                  |
| `format`      | Contêineres, codecs e perfis de qualidade                      |
| `capture`     | Descrição dos pipelines GStreamer                              |
| `clip`        | Nomes de arquivo e comando do ffmpeg                           |
| `recorder`    | Orquestra tudo atrás de traits (backend e exportador)          |
| `gst`, `ffmpeg`, `portal` | Implementações reais: GStreamer, ffmpeg e portal   |
| `labels`      | Textos da interface                                            |

## Testes

```sh
cargo test
```

Os testes de unidade cobrem o núcleo. Há também testes de integração com o GStreamer e o `ffmpeg` reais, que usam uma fonte de teste no lugar do PipeWire e conferem os arquivos gerados em todos os formatos. Se o GStreamer ou o `ffmpeg` não estiverem disponíveis, esses testes são ignorados.

## Limitações e próximos passos

- Ainda **não captura áudio**.
- Codificação apenas por software; encoder por hardware (VA-API) é o próximo passo natural.
- A seleção da tela/janela é pedida a cada início de captura.
- Detecção automática de shiny ou raridade está fora do escopo, pois exigiria analisar a tela quadro a quadro.
