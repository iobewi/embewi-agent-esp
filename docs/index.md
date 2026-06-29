# Embewi Agent — Documentation

Cette documentation s'adresse à deux publics distincts — choisissez votre
section selon votre rôle.

**Agent** — design et fonctionnement interne : architecture, API HTTPS,
configuration, sécurité de production.

**Workload SDK** — développer et déployer une application embarquée :
écrire le code métier, lire la configuration runtime, produire l'artefact OTA.

```{toctree}
:maxdepth: 2
:caption: Agent

Architecture <architecture>
API inbound <api>
Configuration <configuration>
Sécurité de production <embewi-prod-security>
```

```{toctree}
:maxdepth: 2
:caption: Workload SDK

Développer un workload <workload>
Déployer un workload <deploy>
```

---

L'agent implémente le contrat [`embewi`](https://iobewi.github.io/embewi/)
(`v1alpha1`) : piloté par un contrôleur Kubernetes
([embewi-core](https://github.com/iobewi/embewi-core)) via une API HTTPS
authentifiée, il gère les OTA A/B avec selfcheck borné et rollback automatique,
et émet heartbeat et logs vers le Core.

```{mermaid}
graph TB
    subgraph core["embewi-core — Kubernetes"]
        K["McuDeployment · McuNode · McuConfigMap"]
    end

    subgraph esp["embewi-agent-esp — ESP32 C3 / C6 / S3…"]
        OTA["OTA A/B"]
        CFG["McuConfigMap NVS"]
        WL["Workload\n(votre code ici)"]
    end

    core -- "HTTPS :443 · token Bearer" --> esp
    esp -- "HTTPS heartbeat · WSS logs" --> core
```

## Repères

- **Dépôt** : [iobewi/embewi-agent-esp](https://github.com/iobewi/embewi-agent-esp)
  (code source, build, tests, partitions).
- **Contrat Core ↔ Agent** : [iobewi.github.io/embewi](https://iobewi.github.io/embewi/)
  (spécification normative `v1alpha1`, rattachée en submodule `contract/`).
- **Chips supportés** : ESP32-C3 (testé). Compatible C6, S3, H2 (même famille RISC-V).
