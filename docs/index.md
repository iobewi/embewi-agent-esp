# Embewi Agent — Documentation

Firmware **device** (ESP32 / ESP-IDF) du système Embewi : un agent OTA managé
par un runtime Kubernetes ([embewi-core](https://github.com/iobewi/embewi-core),
repo séparé). Cette documentation couvre la **partie embarquée**.

L'agent expose une API HTTPS, s'auto-valide après OTA (rollback bootloader borné
par watchdog), reçoit sa config runtime du Core (McuConfigMap), et émet
heartbeat + logs chiffrés horodatés (NTP).

```{toctree}
:maxdepth: 2
:caption: Documentation

Contrat Core ↔ Agent <embewi-contract-v2>
Conception côté Core <embewi-core-design>
Sécurité production <embewi-prod-security>
```

## Repères

- **Contrat** (`embewi-contract-v2`) : la spec normative de la frontière
  Core ↔ Agent — endpoints, états, OTA, sécurité, config. **Source de vérité.**
- **Conception Core** (`embewi-core-design`) : design des features purement
  Core (rollout de flotte, webhook de validation) — pour le repo Core.
- **Sécurité production** (`embewi-prod-security`) : procédure de durcissement
  (Secure Boot v2, Flash Encryption) et gestion des clés.

> Le code source et le `README` vivent dans le dépôt
> [embewi (agent)](https://github.com/iobewi/embewi).
