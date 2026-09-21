# Archive — spike « workload natif temps réel » (ESP32-C3)

Code mis de côté le 2026-09-21, une fois la question du hard real-time
tranchée (voir `contract/docs/temps-reel-natif.md` : le hard-RT strict est
confié à un domaine dédié, le workload natif reste du soft/firm-RT).
Rien ici n'est compilé par l'agent.

## Contenu

| Chemin | Ce que c'est |
|---|---|
| `workload.rs` | Côté agent : mapping MMU du workload en XIP, IRQ TIMG1 liée à l'ISR du workload, monitor 5 s, mode de mesure 10 kHz (runs de 60 s, rapport), instrumentation (spans flash, prints, activités, compteurs). |
| `workload-poc/` | Le binaire `workload.bin` lié séparément (ISR `workload_timer_isr`, RAM à offsets fixes, `workload.ld`). Build : `cd workload-poc && cargo build --release`. |
| `workload.wat` | Premier spike WASM (wasmi), abandonné : ne compile pas sur riscv32imc. |
| `scripts/rt-load.py` | Charge HTTPS (un handshake par requête) sur l'API admin. Évite `/health` (écrit en NVS). |
| `scripts/ctrl-mock.py` | Faux plan de contrôle TLS (heartbeat + WebSocket de logs). |
| `scripts/select-exp.sh` | Sélection de l'image servie par ESP Web Tools. |
| `images/exp/` | Images des expériences (`baseline`, `nvs`, `print`, `combined`, `poc2d`, `poc2d-b`) + `workload.bin`. ~8 Mo : ne pas versionner tel quel. |
| `instrumented-src/` | Versions **instrumentées** des fichiers modifiés par le spike (`storage.rs`, `heartbeat.rs`, `log_stream.rs`, `bin/main.rs`, `lib.rs`, `stack_usage.rs`, `Cargo.toml`, `build.rs`, `save-image.sh`, `web/manifest.json`), telles qu'elles étaient à l'image `poc2d-b`. |

## Ce qui a été conservé dans le code de l'agent

- `Storage` garde un `Nvs` persistant (plus de scan complet de la partition à
  chaque `get`/`set`), abandonné puis rétabli après un cache invalidé par
  l'accès OTA : `Storage::with_raw_flash` ne le détruit plus (partitions NVS
  et OTA disjointes).
- `ota.rs` utilise `with_raw_flash` (API bornée) au lieu d'un `raw_flash()`.

## Restaurer le spike

1. Recopier les fichiers d'`instrumented-src/` sur ceux de `src/`, `Cargo.toml`,
   `build.rs`, `scripts/save-image.sh`, `web/manifest.json`.
2. Remettre `workload.rs` dans `src/`, `workload-poc/` dans `crates/`,
   les scripts dans `scripts/`.
3. Compiler avec `--features "exp-nvs-cache exp-quiet-print"` (mode mesure 10 kHz).

Attention : les fichiers instrumentés datent d'aujourd'hui ; si `storage.rs`
ou `ota.rs` ont évolué depuis, fusionner à la main plutôt qu'écraser.
