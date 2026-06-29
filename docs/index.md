# Embewi Agent — Documentation

Firmware **device** (ESP32 / ESP-IDF ≥ 5.3) du système Embewi.
L'agent implémente le contrat [`embewi`](https://iobewi.github.io/embewi/)
(`v1alpha1`) : il est piloté par un contrôleur Kubernetes
([embewi-core](https://github.com/iobewi/embewi-core)) via une API HTTPS
authentifiée, gère les OTA A/B avec self-check borné + rollback automatique,
et émet heartbeat + logs chiffrés vers le Core.

```{toctree}
:maxdepth: 2
:caption: Exploitation

Architecture <architecture>
API inbound <api>
Configuration <configuration>
Sécurité production <embewi-prod-security>
```

```{toctree}
:maxdepth: 2
:caption: Workload SDK

Développer un workload <workload>
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
│  │  App workload    │ │
│  │ (votre code ici) │ │
│  └──────────────────┘ │
└──────────────────────┘
```

## Deux audiences, deux sections

**Exploitation** — pour l'opérateur / l'équipe Core :
déploiement, configuration K8s, API HTTPS, sécurité prod.

**Workload SDK** — pour le développeur d'application :
écrire le code métier qui tourne dans l'agent, lire la config,
exposer un service, produire l'artefact OTA.

## Repères

- **Dépôt** : [iobewi/embewi-agent-esp](https://github.com/iobewi/embewi-agent-esp)
  (code source, build, tests, partitions).
- **Contrat Core ↔ Agent** : [iobewi.github.io/embewi](https://iobewi.github.io/embewi/)
  (spécification normative `v1alpha1`, rattachée en submodule `contract/`).
- **Chips supportés** : ESP32-C3 (testé). Compatible C6, S3, H2 (même famille RISC-V).
