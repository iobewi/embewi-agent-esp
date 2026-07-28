# Politique de sécurité

Ce firmware pilote du hardware réel (moteurs, relais, actionneurs GPIO). Une
vulnérabilité ici a un impact potentiellement physique, pas seulement
logiciel — merci de nous laisser le temps de corriger avant toute
divulgation publique.

Politique commune aux trois dépôts du projet — voir aussi
[`embewi-core`](https://github.com/iobewi/embewi-core/blob/main/SECURITY.md)
et [`embewi`](https://github.com/iobewi/embewi/blob/main/SECURITY.md).

## Signaler une vulnérabilité

**Ne pas ouvrir d'issue publique.** Utiliser la divulgation privée GitHub :
onglet **Security** → **Report a vulnerability** (*Private vulnerability
reporting*) sur ce dépôt.

Merci d'inclure : la version/commit concernés, le profil de build (dev ou
prod — `sdkconfig.defaults.prod`), une description de l'impact (accès
inbound non authentifié, contournement OTA/Secure Boot, fuite de
token/clé/CA, etc.), et si possible un scénario de reproduction.

## Délai de réponse

Accusé de réception sous 5 jours ouvrés. Priorité aux problèmes permettant
un accès inbound non authentifié, un contournement du rollback/self-check,
ou la falsification d'une image acceptée par le bootloader.

## Modèle de menace (résumé)

Détail complet : `docs/embewi-prod-security.md` (ce dépôt) et
`contract/docs/embewi-contract-v2.md` §1 (contrat, NORMATIF).

```text
Core verifies for efficiency.
Bootloader verifies for trust.
```

- **Racine de confiance** : le bootloader (Secure Boot v2), pas le Core ni le
  réseau — un attaquant qui atteint l'IP de management d'un device sans
  Secure Boot activé peut écrire une image arbitraire via `/ota/write`.
- **Profil dev vs prod** (`sdkconfig.defaults.prod`, **opt-in uniquement**) :
  Secure Boot v2, Flash Encryption, anti-rollback eFuse, chiffrement NVS,
  vérification du certificat Core sortant (CA embarquée + validité
  temporelle), filtrage IP inbound. Ces opérations eFuse sont
  **irréversibles** — jamais activées par défaut, jamais en dev.
- **Clé de signature Secure Boot** (`secure_boot_signing_key.pem`) : secrète,
  jamais commitée (`.gitignore`), gérée hors dépôt (coffre/HSM/CI). Sa fuite
  permet à un attaquant de signer des images acceptées par tous les devices
  déjà verrouillés sur cette clé.
- **CA du Core** (`main/core_ca.pem`) : publique mais spécifique à
  l'environnement, hors dépôt également — le build de production échoue
  volontairement si elle est absente plutôt que d'embarquer une CA
  silencieusement fausse.
- **Provisioning (portail captif AP)** : le point le plus faible par
  nécessité — aucun secret partagé n'existe avant l'enrôlement. Le portail
  est servi en HTTPS (bloque le sniffing passif) ; MITM actif sur l'AP
  ouvert reste un résiduel accepté pour le MVP.
- **Canal de détresse NTP** (`embewi_tls_relaxed.c`) : tant que l'horloge
  n'est pas synchronisée, seule la validité temporelle du certificat Core
  est tolérée sur le heartbeat/logs HTTPS — la chaîne de confiance et le CN
  restent vérifiés sans exception. Le streaming logs WebSocket n'est pas
  couvert (déjà best-effort par conception).

## Versions supportées

Pas encore de ligne de versions stabilisée (pré-v1.0, protocole `v1alpha1`).
Une fois taggé, seule la dernière version mineure reçoit des correctifs de
sécurité.
