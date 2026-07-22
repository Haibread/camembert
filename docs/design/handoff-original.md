# Handoff — Outil d'analyse d'espace disque (successeur de ncdu)

> **Statut du document** : synthèse d'une session d'idéation. Tout ce qui suit est un ensemble d'hypothèses et de pistes jugées prometteuses, **pas un cahier des charges figé**. Chaque choix est challengeable ; les points les plus incertains sont marqués ⚠️. Les sections « Écarté » documentent le raisonnement pour éviter de re-débattre sans élément nouveau — mais un élément nouveau suffit à rouvrir.

---

## 1. Positionnement

**Constat de départ** : « plus rapide que ncdu » n'est plus un différenciant. gdu, dua-cli et diskonaut ont déjà parallélisé le scan, et ncdu 2 (Zig) a rattrapé l'essentiel. La vitesse brute est un prérequis, pas un argument.

**Thèse du projet** (à valider par l'usage) : la différenciation se joue sur deux axes que personne ne couvre bien :

1. **Répondre aux vraies questions** — pas « qu'est-ce qui est gros » mais « qu'est-ce qui a grossi » (diff), « qu'est-ce que je peux réellement libérer » (récupérable ≠ taille), « qu'est-ce qui est gros ET froid » (âge).
2. **Corriger ce que les autres outils affichent de faux** — hardlinks, extents partagés btrfs, fichiers supprimés-mais-ouverts, slack, inodes, quotas, compression FS.

**Cibles** : Linux d'abord (serveurs inclus — d'où l'exigence de portabilité), macOS/Windows ensuite.

---

## 2. Hypothèses d'architecture

| Hypothèse | Rationale | Challengeable ? |
|---|---|---|
| Rust | Écosystème TUI/GUI mûr, binaire statique musl facile, contrôle mémoire fin | Zig serait défendable (ncdu 2 l'a fait) ; Rust retenu pour l'écosystème |
| Cœur en lib pure, TUI et GUI comme frontends séparés | Permet GUI plus tard sans refonte, feature flags pour ne pas alourdir le binaire terminal | Peu de raisons de revenir dessus |
| **Pas de daemon obligatoire** | Décision explicite : un binaire qu'on `scp` sur un serveur et qui marche. Le cache/index arrive plus tard comme option, jamais comme prérequis | Ferme la porte à l'index fanotify temps-réel (façon Everything/USN). Assumé, mais c'était l'autre grande piste |
| Binaire statique `x86_64-unknown-linux-musl` + `aarch64` | Portabilité serveur, ARM partout désormais | Non |
| io_uring brut (crate `io-uring`), pas de runtime async | Pas besoin de tokio pour ce profil de charge | À revoir si le scan distant complexifie l'I/O réseau |
| ⚠️ egui pour la GUI future | Un treemap = des rectangles, egui suffit largement, avec culling/LOD au-delà de quelques milliers de nœuds | Choix très peu engagé à ce stade, la GUI est lointaine |

---

## 3. Moteur de scan — pistes de perf

Par ordre de rendement estimé :

1. **Traversée relative aux descripteurs** : `openat(dirfd, name, O_DIRECTORY)` + `getdents64`, jamais de chemin absolu reconstruit. Évite la résolution de chemin à chaque niveau, pas de limite `PATH_MAX`, immunité aux symlinks hostiles.
2. **`d_type` de getdents64** pour distinguer fichiers/répertoires sans `stat` (fallback `stat` si `DT_UNKNOWN`, certains FS le renvoient).
3. **`statx` batché via io_uring** (kernel 5.6+) pour les tailles. C'est le levier que quasiment personne n'exploite. **Fallback obligatoire dès le v1** : seccomp Docker par défaut bloque io_uring, gVisor aussi, certaines distros le désactivent par sysctl → détection runtime, repli sur pool de threads. Sans ce fallback le binaire « portable » est inutilisable là où on en a le plus besoin.
4. **Threading adaptatif** : file de dirfds avec vol de tâches, ~2-4× les cœurs sur NVMe, mais lire `/sys/block/*/queue/rotational` et retomber à 1-2 threads sur HDD (sinon seek-fest).
5. **Layout mémoire** : arena `Vec<Node>` + indices `u32` + interning des noms. Sur 10 M de fichiers, différence entre ~300 Mo et 1,5 Go de RSS.
6. ⚠️ Plus tard, si besoin : APIs bulk natives (`getattrlistbulk` macOS, MFT Windows — le secret de WizTree, admin requis).

**Piège de benchmark** : les comparatifs publics tournent en cache dentry chaud et mesurent le CPU. Benchmarker aussi à froid (`echo 3 > /proc/sys/vm/drop_caches`), où le stockage domine.

**Correctitude du scan** :
- Frontières de montage : comparer `st_dev`, ne pas franchir par défaut (flag pour forcer), exclure `/proc`, `/sys`, autofs non montés.
- Hardlinks : dédup par `(dev, ino)` — structure compacte, ce set grossit vite.
- Sémantique des tailles à trancher **tôt** : apparente (`st_size`) vs blocs réels (`st_blocks`), sparse, compression btrfs/zfs, reflinks/clones APFS. C'est là que tous les outils divergent et se font troller. Proposition : afficher les deux, blocs réels par défaut.

---

## 4. La pièce maîtresse : le format de dump

Presque toutes les features convergent dessus : diff, cache, scan distant, export HTML, mode CI. **À figer en premier**, tout le reste s'y branche.

Exigences pressenties :
- Streamable (écriture pendant le scan, lecture partielle).
- Contient assez de métadonnées pour le diff : tailles, mtime, uid, inode/dev (hardlinks), compteurs d'inodes, erreurs de lecture.
- Versionné dès le départ.
- **Import du format dump ncdu** (documenté, ~un après-midi) : levier d'adoption disproportionné, tous les détenteurs d'exports ncdu peuvent tester sans rescanner.
- ⚠️ Format exact (JSON lines ? binaire + index ?) : non tranché. Contrainte : le diff de deux dumps de 10 M d'entrées doit tenir en mémoire raisonnable.

**Honnêteté du cache** : sans fanotify, un cache n'est pas fiable (le mtime d'un répertoire ne reflète pas les changements profonds ni les fichiers qui grossissent). Position retenue : afficher instantanément le dernier scan **marqué comme daté**, rafraîchir en tâche de fond. Ne jamais le vendre comme exact.

---

## 5. Features — par priorité pressentie

### Cœur différenciant

**UI navigable pendant le scan** — parti pris d'architecture plus que feature : arbre construit en incrémental, navigation immédiate, tailles qui se remplissent et se re-trient en direct. Le ressenti passe de « rapide » à « instantané ». Impose que le modèle de données supporte les mises à jour concurrentes des agrégats parents → **à décider avant d'écrire le moteur**.

**Diff entre deux scans** — arbre de deltas trié par croissance. Répond à « qu'est-ce qui a bougé depuis hier », la vraie question en incident. Quasi gratuit une fois le format de dump posé. Personne ne le propose. **Meilleur rapport valeur/effort identifié.**

**Colonne « libérable » distincte de « taille »** — corrige le mensonge de tous les outils :
- Fichiers supprimés mais ouverts (le classique `df` plein / `du` vide) : scan de `/proc/*/fd`, symlinks `(deleted)`, avec PID coupable affiché.
- btrfs : extents partagés avec snapshots via `FIEMAP_EXTENT_SHARED` (approche de `btrfs fi du`). ZFS : pas d'API par fichier → ne rien afficher plutôt qu'inventer.
- Hardlinks dont les frères sont hors de la sélection.

**Dimension âge** — le bon candidat à suppression est gros **et** froid. Tri/score (taille × ancienneté). Attention : `relatime` = atime à granularité journalière, `noatime` = rien → détecter et retomber sur mtime en l'annonçant.

### Vues et requêtes

- **Vue plate globale** : top N fichiers de tout l'arbre, hors hiérarchie.
- **Agrégation par motif** : `node_modules` = 14 Go cumulés, `*.log` = 3 Go. Les gens pensent par catégorie, pas par emplacement.
- **Filtre qui ré-agrège** : filtrer `*.mp4` recalcule tout l'arbre sur le sous-ensemble ; combiné avec « plus vieux que 6 mois », ça devient un langage de requête. Même mécanique d'agrégation que le scan, réappliquée.
- **Répartition par propriétaire (uid/gid)** : `st_uid` est déjà dans chaque statx, vue quasi gratuite, très demandée sur machines partagées.

### Vérité des chiffres

- **Inodes** : compteur par répertoire + alerte à l'approche de `f_files` (statvfs). Le mode de panne que personne n'affiche.
- **Slack** : écart apparent/réel sur les masses de petits fichiers (`st_blocks` déjà disponible, gratuit).
- **Quotas** : `quotactl`, project quotas XFS — sur machine partagée la limite n'est pas le disque.
- **Comptabiliser l'illisible** : « 340 répertoires non lus (permission), ~12 Go non comptés ». Condition de la confiance dans le total.
- ⚠️ **Ratio de compression btrfs/zfs** (façon `compsize`) : niche, cohérent avec la thèse, priorité basse.

### Intégration et distribution

- **Composable façon fzf** : sortir la sélection sur stdout en quittant → `rm $(mondu --print /)`. Trois lignes de code.
- **Mode non-interactif** : `--top 20 --json`, seuil avec code retour non nul → sonde de monitoring.
- **Scan distant auto-déployé** : `mondu ssh://serveur:/var` pousse le binaire statique dans `/tmp` distant, scanne là-bas, rapatrie le dump, ouvre en local. Découle directement du choix binaire-statique-sans-daemon. `ncdu -o- | ssh` existe mais en bricolage manuel.
- **Export rapport HTML** : page statique auto-contenue (treemap + top fichiers) depuis un dump, à coller dans un ticket. Autant de la distribution que de la feature.

### Confort et sécurité

- **Watch mode sans daemon** : tant que le TUI est ouvert, watches inotify sur les seuls répertoires dépliés (quelques dizaines, pas de problème de limite). Sensation du daemon indexé sans son coût, meurt avec le processus.
- **Garde-fous de suppression** : corbeille marquer-puis-valider (comme dua) + mieux que les autres : refus des points de montage, avertissement si fichier ouvert par un process (réutilise le code `/proc/*/fd`), option spec XDG Trash au lieu d'`unlink`.
- ⚠️ **Recettes de nettoyage** : base statique de chemins connus (`/var/log/journal` → `journalctl --vacuum-time=`, `~/.cache/pip` → `pip cache purge`, overlay2 Docker → **ne pas supprimer à la main**, caches apt/pacman, `target/` Cargo…). **Jamais exécuter, seulement afficher la commande propre.** Coût réel : maintenance de la liste → la garder courte. Distinguer régénérable vs vraie donnée.

---

## 6. Écarté (avec le raisonnement, pour pouvoir le contester)

| Idée | Raison de l'écarter |
|---|---|
| Déduplication par hash de contenu | Change la nature de l'outil, double la complexité pour un usage rare — c'est un autre projet |
| Attribution Docker (images/volumes/cache) | Très demandé mais couplage aux internes de `/var/lib/docker` qui cassent régulièrement |
| Prédiction de saturation à J+n | Jolie en démo, fausse dès qu'une rotation de logs existe |
| Intégrations cloud (S3, etc.) | Facturation par appel API = modèle de coût sans rapport avec un scan FS |
| Protocoles graphiques terminal (kitty/sixel) | Spectaculaire en démo, cauchemar de compatibilité |
| Treemap TUI, corbeille marquer/valider | À faire, mais déjà pris (diskonaut, dua) — ce ne sont pas des arguments de différenciation |
| Daemon d'index fanotify (`FAN_MARK_FILESYSTEM`, 4.20+) | Écarté par le choix « portable sans daemon ». **Le plus contestable de cette liste** : c'était l'autre grand créneau vide (équivalent Linux d'Everything/USN). Réouvrable en mode opt-in un jour |

---

## 7. Questions ouvertes

1. Format de dump exact (cf. §4) — **bloquant, à trancher en premier**.
2. Structure de données pour les mises à jour concurrentes d'agrégats pendant le scan (bloquant pour l'UI-pendant-scan).
3. Nom du projet.
4. Framework TUI (ratatui est le défaut évident en Rust, non challengé).
5. Politique par défaut apparente vs blocs réels (proposition : blocs, avec bascule).
6. Périmètre exact du MVP.

---

## 8. Proposition de découpage (à challenger)

Le backlog ci-dessus représente ~2 ans de travail. Découpage proposé, non validé :

- **MVP** : moteur de scan (openat/getdents64/d_type, threading adaptatif, fallback sans io_uring) + TUI navigable pendant le scan + format de dump v1 + suppression sûre + comptage de l'illisible.
- **Vague 2** : diff, colonne libérable (deleted-but-open + hardlinks d'abord, btrfs ensuite), vue plate, tri par âge, mode non-interactif, import ncdu.
- **Vague 3** : filtre ré-agrégé, agrégation par motif, vue par propriétaire, inodes/slack/quotas, composable stdout, statx io_uring.
- **Vague 4** : scan distant ssh, export HTML, watch mode, recettes, cache daté.
- **Plus tard** : GUI, macOS, Windows, compression FS.

Logique du découpage : le MVP prouve la thèse « instantané + honnête », la vague 2 apporte les deux features tueuses (diff, libérable), le reste s'empile sur le format de dump.
