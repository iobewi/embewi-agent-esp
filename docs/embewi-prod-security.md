# Sécurité de production

> Met en œuvre les exigences du contrat §1 (racine de confiance) **sans toucher
> au flux de développement**. Tout est porté par un profil de build séparé et opt-in.
>
> ⚠️ **Opérations eFuse IRRÉVERSIBLES.** Lire entièrement avant le premier flash.
> Une erreur ici brique définitivement la capacité à reflasher/déboguer le device.
> **Tester d'abord sur un device sacrifiable.**

---

## Développement vs production — le principe

| | Développement (défaut) | Production (`sdkconfig.defaults.prod`) |
|---|---|---|
| Secure Boot v2 | ❌ off | ✅ bootloader refuse les images non signées |
| Chiffrement flash | ❌ off | ✅ flash chiffrée (mode RELEASE) |
| Anti-rollback | ❌ off | ✅ downgrade sécurité bloqué (eFuse) |
| Mode download ROM | ouvert | secure DL mode |
| Chiffrement NVS | ❌ off | ✅ schéma flash-enc (partition `nvs_keys`) |
| Vérification cert du Core (sortant) | ❌ skip | ✅ CA embarquée (`CONFIG_EMBEWI_VERIFY_CORE_CERT`) |
| Reflash libre | ✅ | ❌ (par design) |

Le développement reste rapide et reflashable. La sécurité ne s'active que via
le profil de production, jamais par défaut.

> **Statut : profil de production build-validé.** Un build complet a confirmé
> que tout l'empilement compile et signe (bootloader Secure Boot v2 inclus).
> **Reste à valider sur silicium** : le burn eFuse au premier flash (non testable
> hors hardware).

---

## Étape 1 — Prérequis (une fois par environnement)

### Clé de signature Secure Boot (secrète — jamais dans le dépôt)

```bash
idf.py secure-generate-signing-key --version 2 --scheme rsa3072 \
       secure_boot_signing_key.pem
```

- Garder cette clé dans un coffre (Vault, HSM, gestionnaire de secrets CI).
- Sa perte = impossibilité de signer de futurs OTA pour les devices déjà
  verrouillés. Sa fuite = un attaquant peut signer des images acceptées.
- `*.pem` est déjà dans `.gitignore` — vérifier qu'elle n'est jamais commitée.

### CA du Core (publique — authentifie les flux sortants)

Le device vérifie le serveur du Core (`/heartbeat`, `/logs` WS) contre cette CA.
Fournir le certificat de l'autorité qui signe le cert serveur du Core
(cert-manager interne, CA d'entreprise, etc.) :

```bash
cp <votre-core-ca>.pem main/core_ca.pem
```

- Ce n'est **pas** un secret (cert public), mais il est **spécifique à
  l'environnement** → gardé hors dépôt (`.gitignore` `*.pem`).
- Absent au build de production → le build **échoue clairement** (volontaire :
  pas d'embarquement d'une CA silencieusement fausse).

---

## Étape 2 — Build de production

Le profil de production s'**empile** sur `sdkconfig.defaults` (il ne le remplace pas) :

```bash
idf.py -B build-prod \
       -DSDKCONFIG=build-prod/sdkconfig.prod \
       -DSDKCONFIG_DEFAULTS="sdkconfig.defaults;sdkconfig.defaults.prod" \
       build
```

`-B build-prod` isole les artefacts → le `sdkconfig` de développement n'est
**jamais** écrasé. Les deux builds coexistent ; `build-prod/` est gitignoré.

> Pour un build de **validation sans flasher**, une clé de signature et une
> `core_ca.pem` jetables suffisent — le build ne touche aucun eFuse (le
> verrouillage n'arrive qu'au flash/premier démarrage).

---

## Étape 3 — Premier flash et verrouillage eFuse

Au **premier démarrage** d'une image Secure Boot + Flash Enc, le bootloader brûle
les clés dans les eFuse et chiffre la flash. Ce démarrage est **plus long** et
**ne doit pas être interrompu** (coupure d'alimentation = device potentiellement
briqué).

```bash
idf.py -B build-prod -p <PORT> flash monitor
```

Après ce démarrage :
- l'UART ne peut plus lire la flash en clair (mode RELEASE) ;
- seules les images signées avec `secure_boot_signing_key.pem` démarrent ;
- les OTA suivants (via le Core) doivent livrer des binaires **signés** — la
  chaîne OTA du contrat (§3) reste identique, le bootloader ajoute juste la
  vérification de signature à l'activation.

> **Build reproductible pour OTA.** Les binaires OTA poussés par le Core doivent
> être signés avec la même clé. Le Core signe l'artefact avant `PUT /ota/write` ;
> le device vérifie au démarrage (§1 : « Core verifies for efficiency, bootloader
> verifies for trust »).

---

## Étape 4 — Anti-rollback : discipline de version

`CONFIG_BOOTLOADER_APP_SECURE_VERSION` (défaut 0) est gravé/incrémenté dans les
eFuse. Règle opérationnelle :

```text
- Incrémenter secure_version UNIQUEMENT pour un correctif de sécurité qui doit
  interdire le retour à l'image vulnérable.
- Une fois le device démarré sur secure_version=N, il REFUSE toute image < N.
  Irréversible. Ne pas incrémenter à la légère (chaque valeur consomme un bit
  eFuse, le nombre est limité).
- Les mises à jour fonctionnelles normales gardent le même secure_version.
```

---

## Checklist avant déploiement terrain

```text
[ ] secure_boot_signing_key.pem en coffre, hors dépôt, sauvegardée
[ ] main/core_ca.pem = CA réelle du Core de l'environnement cible
[ ] build de production testé sur un device SACRIFIABLE d'abord
[ ] OTA signé vérifié de bout en bout (prepare → write → activate → démarrage)
[ ] rollback bootloader validé sur image de production (couper le selfcheck exprès)
[ ] secure_version aligné avec la politique de sécurité
[ ] mode download ROM : secure (défaut) ou disabled selon le modèle de menace
```

---

## Voir aussi

- `embewi-contract-v2.md` §1 — modèle de sécurité (le « pourquoi »).
- `sdkconfig.defaults.prod` — les options exactes (le « quoi »).
