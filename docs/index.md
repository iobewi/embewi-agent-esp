# Embewi Agent — Documentation

Firmware **device** (ESP32 / ESP-IDF ≥ 5.3) du système Embewi.
L'agent implémente le contrat [`embewi`](https://iobewi.github.io/embewi/)
(`v1alpha1`) : il est piloté par un contrôleur Kubernetes
([embewi-core](https://github.com/iobewi/embewi-core)) via une API HTTPS
authentifiée, gère les OTA A/B avec self-check borné + rollback automatique,
et émet heartbeat + logs chiffrés vers le Core.

```{toctree}
:maxdepth: 2
:caption: Agent

Architecture <architecture>
API inbound <api>
Configuration <configuration>
Workloads <workload>
Sécurité production <embewi-prod-security>
```

## Vue d'ensemble

```text
┌─────────────────────────────────────────────────┐
│              embewi-core (Kubernetes)            │
│  McuDeployment · McuNode · McuConfigMap          │
└──────────┬────────────────────────┬─────────────┘
           │ inbound HTTPS :443     │ outbound
           │ (token Bearer)         │ HTTPS heartbeat
           ▼                        │ WSS logs
┌──────────────────────┐            │
│   embewi-agent-esp   │◄───────────┘
│   ESP32 (C3/C6/S3…)  │
│                       │
│  ┌─────┐ ┌─────────┐ │
│  │ OTA │ │McuConfig│ │
│  │ A/B │ │Map NVS  │ │
│  └─────┘ └─────────┘ │
│  ┌──────────────────┐ │
│  │ App workload     │ │
│  │ (button/rainbow) │ │
│  └──────────────────┘ │
└──────────────────────┘
```

## Fonctionnalités

- **Provisioning** : portail captif Wi-Fi HTTPS (AP `embewi-XXXX`, 10 min),
  token affiché une seule fois, IP statique ou DHCP.
- **API HTTPS :443** : 11 endpoints inbound, auth Bearer par node à temps
  constant sur chaque appel.
- **OTA A/B** : write chunké, digest SHA-256 incrémental (PSA crypto),
  reprise après coupure (`Content-Range` in-session), idempotence NVS.
- **Self-check borné** : validation 15 s après OTA → `mark_valid` ou rollback
  bootloader ; check storage réel (canary NVS).
- **McuConfigMap** : config runtime poussée par le Core (GPIO, NTP…), découplée
  du binaire OTA.
- **Heartbeat** : POST HTTPS toutes les 5 s (état, RSSI, heap, temp SoC,
  stack HWM min des tâches).
- **Streaming logs** : tous les `ESP_LOGx` capturés → WebSocket `wss` vers le
  Core ; événements OTA/lifecycle via HTTPS POST.
- **NTP/SNTP** : horloge synchronisée au boot — pré-requis du TLS authentifié
  en prod.
- **Sécurité** : Secure Boot v2 + Flash Encryption + anti-rollback eFuse
  (profil opt-in).

## Repères

- **Dépôt** : [iobewi/embewi-agent-esp](https://github.com/iobewi/embewi-agent-esp)
  (code source, build, tests, partitions).
- **Contrat Core ↔ Agent** : [iobewi.github.io/embewi](https://iobewi.github.io/embewi/)
  (spécification normative `v1alpha1`, rattachée en submodule `contract/`).
- **Chips testés** : ESP32-C3. Compatible C6, S3, H2 (même famille RISC-V).
