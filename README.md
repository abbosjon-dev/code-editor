# ◆ Zenith

**Fast. Extensible. AI-native.** — Rust tilida yozilgan, VS Code uslubidagi kod muharriri.
Claude Code, Codex, Gemini CLI, Aider kabi AI agentlar to'g'ridan-to'g'ri editor ichida ishlaydi.
macOS, Windows va Linux'da ishlaydi.

![Zenith screenshot](docs/screenshot.png)

## Imkoniyatlar

| | |
|---|---|
| ⚡ **Tezlik** | Rust + GPU orqali chizish (egui/OpenGL). Faqat ko'rinadigan qatorlar chiziladi; sintaksis ranglash inkremental va har kadrda vaqt bilan cheklangan — katta fayllarda ham UI qotmaydi. |
| ✳ **AI agentlar** | O'ng tomondagi AI panelda haqiqiy agent CLI'lari ishlaydi: **Claude Code**, OpenAI **Codex**, **Gemini CLI**, **Aider**, **OpenCode** yoki o'zingizning har qanday CLI'ingiz. "Ask AI" (`Ctrl+Shift+I`) belgilangan kodni fayl yo'li bilan agentga yuboradi. Savolda `@` yozsangiz — loyiha fayllari ro'yxati chiqadi (`@src/main.rs` kabi boshqa fayllarni ham kontekstga qo'shish). Agent faylni o'zgartirsa, editor uni darhol yuklaydi va **o'zgarishlarni diff ko'rinishida** ko'rsatadi — har birini qoldirish yoki bitta tugma bilan qaytarish mumkin. **⧉ Changes** oynasi esa agent ishga tushgandan beri o'zgargan *barcha* fayllarni (yangi, o'zgargan, o'chirilgan) bir joyda yonma-yon diff bilan ko'rsatadi: har faylni yoki hammasini **Keep / Revert**. |
| 🧩 **Pluginlar** | Plugin — istalgan tildagi dastur (Python, JS, Go, Rust…). Alohida jarayonda ishlaydi, shuning uchun plugin xatosi editorni hech qachon qotirmaydi yoki buzmaydi. Yoki **WebAssembly** plugin — editor ichida sandbox'da ishlaydi (fayl va tarmoqqa kira olmaydi), bitta `.wasm` fayl hamma OS'da. Pluginlar buyruqlar, **o'z yon panellarini** (masalan, TODO Tree) va **status qatori elementlarini** qo'sha oladi. **Marketplace** — Plugins panelida pluginlarni qidirish, bir tugma bilan o'rnatish, yangilash va o'chirish. [Qo'llanma →](docs/PLUGINS.md) |
| ⌨ **Terminal** | To'liq terminal emulyatori (PTY: Unix'da openpty, Windows'da ConPTY), 256/truecolor, scrollback, sichqoncha, nusxa olish, bir nechta tab, **ikki terminal yonma-yon** (‖ tugmasi yoki *Terminal: Split Terminal*; AI panelda ikki agent yonma-yon). `Ctrl+bosish` bilan chiqishdagi `fayl:qator:ustun` havolalarini muharrirda va URL'larni brauzerda ochadi. |
| 📝 **Muharrir** | **Multi-cursor** (Ctrl+D, Alt+bosish, Ctrl+Alt+↑↓), **ustun bo'yicha belgilash** (`Shift+Alt+sudrash`), **split editor** (`Ctrl+\`), **kod bloklarini yig'ish** (gutter'dagi ▾ yoki `Ctrl+Shift+[` / `]`), **so'zlarni o'rash** (`Alt+Z`), **Markdown preview** (`Ctrl+Alt+P`, rasmlar bilan), qator ichida xato xabarlari, sintaksis ranglash (~200 til), avtomatik qavs yopish, aqlli indent, bir nechta tab, undo/redo, qidirish va almashtirish, minimap, indent chiziqlari, qavs juftini ko'rsatish, qatorni ko'chirish/nusxalash, izoh qo'yish. |
| 🌳 **Tree-sitter** | Rust, Python, JavaScript/TypeScript (JSX/TSX), Go, C/C++, JSON, Bash, HTML, CSS uchun sintaksis daraxti asosida aniq ranglash va **sintaktik yig'ish** (folding) — har tahrirda inkremental qayta tahlil (20 ming qatorgacha bo'lgan faylda bir tugma ~5 ms). Boshqa tillar uchun TextMate grammatikalari (syntect). Sozlamalar → Editor → Tree-sitter. |
| 🧠 **LSP (til serverlari)** | Avtomatik to'ldirish, xatolar va ogohlantirishlar (qizil to'lqinli chiziq), hover hujjatlari, ta'rifga o'tish (`F12` / `Ctrl+bosish`), havolalarni topish (`Shift+F12`), nomni hamma joyda o'zgartirish (`F2`), formatlash (`Shift+Alt+F`, ixtiyoriy saqlashda formatlash), parametr maslahatlari, tezkor tuzatishlar (`Ctrl+.` / 💡), fayldagi belgiga o'tish (`Ctrl+T`), butun loyihadagi belgilar (`Ctrl+Shift+T`), **inlay hints** — o'zgaruvchi turlari va parametr nomlari qator ichida. **Semantik ranglash** — til serveri belgining nima ekanini aytadi (parametr, maydon, tur, makros…) va rang shunga qarab tanlanadi. O'rnatilgan bo'lsa avtomatik ishga tushadi: **rust-analyzer**, **pyright**, **typescript-language-server**, **gopls**, **clangd** va boshqalar. |
| ⎇ **Git** | Gutter'da o'zgarish belgilari (qo'shilgan / o'zgargan / o'chirilgan), Source Control paneli (`Ctrl+Shift+G`): stage/unstage, commit (`Ctrl+Enter`), push/pull, stash, merge konfliktlarini hal qilish, fayl daraxtida rangli holat. **✳ Commit with AI** — agent diff'ni ko'rib, commit'ni o'zi yozadi. |
| 🌐 **SSH orqali ishlash** | *File → Open Remote Folder (SSH)…* — uzoq serverdagi papkani ochish: fayllar mahalliy nusxada tez tahrirlanadi, saqlash serverga yuklanadi, terminal va AI agentlar serverning o'zida ishlaydi. |
| 🗂 **Bir nechta papka** | Bir oynada bir nechta loyiha papkasi (multi-root): Explorer, tezkor ochish va fayllarda qidirish hammasini qamraydi; har bir papka o'z Git repo'si (Source Control'da repo tanlash, gutter/blame/branch) va o'z til serveriga ega; sessiyada saqlanadi. |
| 🔍 **Navigatsiya** | Buyruqlar palitrasi (`Ctrl+Shift+P`), fuzzy fayl qidirish (`Ctrl+P`), fayllar ichida qidirish va almashtirish (`Ctrl+Shift+F`: regex, butun so'z, katta-kichik harf, `$1` guruhlar, oldindan ko'rish; `.gitignore`'ni hisobga oladi), qatorga o'tish. |
| 🎨 **Chiroyli UI** | Zenith Midnight (qorong'i) va Daylight (yorug') temalari, 30+ sintaksis temasi, JetBrains Mono + Inter shriftlari. |

## Yuklab olish

[Releases](https://github.com/abbosjon-dev/code-editor/releases) sahifasidan:

| OS | Fayl | O'rnatish |
|---|---|---|
| **macOS** (Apple Silicon / Intel) | `Zenith-…-macos-arm64.dmg` / `…-x86_64.dmg` | DMG'ni oching, Zenith'ni Applications'ga torting. Imzo sertifikatisiz yig'ilgan bo'lsa: birinchi marta **o'ng tugma → Open** (yoki `xattr -dr com.apple.quarantine /Applications/Zenith.app`). |
| **Windows** | `Zenith-…-x86_64.msi` yoki `zenith-…-windows-x86_64.zip` | MSI — o'rnatuvchi (Start menyu, `PATH`, o'chirish Sozlamalardan); ZIP — `zenith.exe` ni to'g'ridan-to'g'ri ishga tushiring. |
| **Har qanday Linux** | `Zenith-…-x86_64.AppImage` | `chmod +x Zenith-*.AppImage && ./Zenith-*.AppImage` |
| **Ubuntu / Debian** | `zenith_…_amd64.deb` | `sudo apt install ./zenith_…_amd64.deb` |
| **Boshqa Linux** | `zenith-…-linux-x86_64.tar.gz` | Arxivni oching va `./install.sh` (foydalanuvchi uchun, `~/.local`ga). |

Paketlarni o'zingiz yig'ish: `bash packaging/package.sh` (natija `dist/` papkasida; AppImage uchun
`appimagetool`, MSI uchun WiX — `dotnet tool install --global wix`).

**Kod imzosi (ixtiyoriy).** Release workflow quyidagi repository secret'lari bo'lsa imzolaydi, bo'lmasa o'tkazib yuboradi:
macOS — `APPLE_CERTIFICATE` (Developer ID .p12, base64), `APPLE_CERTIFICATE_PASSWORD`, `MACOS_SIGN_IDENTITY`,
notarizatsiya uchun `APPLE_ID`, `APPLE_TEAM_ID`, `APPLE_APP_PASSWORD`; Windows — `WINDOWS_CERTIFICATE` (.pfx, base64),
`WINDOWS_CERTIFICATE_PASSWORD`. Mahalliy yig'ishda xuddi shu nomdagi muhit o'zgaruvchilari ishlatiladi
(`packaging/package.sh` boshidagi izohga qarang).

**Yangilanishlar.** Zenith kuniga bir marta GitHub'dagi oxirgi release'ni tekshiradi va yangi versiya chiqsa status
qatorida **⬆ Update** ko'rsatadi. Uni bossangiz — **Install and restart**: Zenith qanday o'rnatilgan bo'lsa, o'shanga mos
paketni yuklab oladi (AppImage, tar.gz, ZIP, macOS DMG ichidagi .app, MSI), GitHub bergan SHA-256 bilan tekshiradi,
o'rnatadi va qayta ishga tushiradi (MSI uchun o'rnatuvchi ochiladi; .deb uchun `sudo apt install …` buyrug'i
ko'rsatiladi). Siz so'ramaguningizcha hech narsa o'rnatilmaydi. *Help → Check for Updates* — qo'lda tekshirish;
Sozlamalar → Advanced'da o'chirish mumkin.

## Manba koddan ishga tushirish

Talab: [Rust](https://rustup.rs) (stable).

```bash
git clone https://github.com/abbosjon-dev/code-editor
cd code-editor
cargo run --release            # yoki: cargo run --release -- /papka/yo'li
```

Tayyor binar fayl: `target/release/zenith` (Windows'da `zenith.exe`).

**Linux** uchun qo'shimcha tizim kutubxonalari (Debian/Ubuntu):

```bash
sudo apt install libxkbcommon-x11-0 libxkbcommon-dev libgl1-mesa-dev libwayland-dev libx11-dev
```

`v*` teg qo'yilganda GitHub Actions avtomatik ravishda macOS (Intel + Apple Silicon), Windows va Linux uchun relizlarni yig'adi.

## AI agentlardan foydalanish

1. Agent CLI'sini o'rnating, masalan Claude Code: `npm install -g @anthropic-ai/claude-code`
   (yoki chap paneldagi ✳ **AI Agents** bo'limida "Install" tugmasini bosing).
2. Papkani oching (`Ctrl+Shift+O`), so'ng `Ctrl+Shift+P` → **AI: Start Claude Code**.
3. Kodni belgilang va `Ctrl+Shift+I` bosing — savolingiz kod bilan birga agentga yuboriladi. `@` bilan boshqa fayllarni ham eslatish mumkin (↑↓ tanlash, Tab/Enter qo'yish).
   O'ng tugma menyusida ham: *Explain / Find Bugs / Write Tests / Refactor*.

O'z agentingizni qo'shish (`settings.toml`, `Ctrl+,`):

```toml
default_agent = "claude"

[[agents]]
id = "my-agent"
name = "My Agent"
command = "my-agent-cli"
args = ["--model", "best"]
```

Editorni yopib qayta ochganingizda ochiq fayllar (kursor joyi bilan), panellar, terminallar va AI agentlar tiklanadi;
Claude Code o'zining oxirgi suhbatini `--continue` bilan davom ettiradi (agentga `resume_args` bilan o'zingiz ham qo'shishingiz mumkin).

API kalitlarni sozlamalarga yozmang — agentlar sizning shell muhitingizdagi o'zgaruvchilarni meros qilib oladi.

## AI o'zgarishlarini ko'rib chiqish

Agent (masalan Claude Code) ochiq faylni o'zgartirsa, tepada banner chiqadi: **Review diff** — har bir o'zgarishni
ko'rish va kerak bo'lmaganini **↺ Revert this change** bilan qaytarish; **Keep** — hammasini qabul qilish;
**Revert all** — avvalgi holatga qaytarish. Saqlanmagan tahrirlaringiz bo'lsa, ular ustiga yozilmaydi.

![Review](docs/review.png)

## Til serverlari (LSP)

Kerakli server o'rnatilgan bo'lsa, faylni ochishingiz bilan ishlay boshlaydi — hech narsa sozlash shart emas.
Holatni **Plugins** panelidagi *Language servers* bo'limida ko'rasiz (o'rnatilmaganlarini ⬇ tugmasi bilan o'rnatish mumkin).

![LSP](docs/lsp.png)

| Til | Server | O'rnatish |
|---|---|---|
| Rust | rust-analyzer | `rustup component add rust-analyzer` |
| Python | pyright | `npm install -g pyright` |
| TypeScript / JavaScript | typescript-language-server | `npm install -g typescript typescript-language-server` |
| Go | gopls | `go install golang.org/x/tools/gopls@latest` |
| C / C++ | clangd | paket menejeri orqali |

Saqlashda avtomatik formatlash: `settings.toml` da `format_on_save = true`.

Boshqa server qo'shish (`settings.toml`):

```toml
[[language_servers]]
name = "pylsp"
command = "pylsp"
languages = ["python"]
```


![Inlay hints](docs/inlay-hints.png)

## Testlar

**Testing** paneli (✓ belgisi) loyihadagi testlarni topadi: **Rust** (`cargo test` — lib, bin va `tests/`
integratsion testlari) va **Python** (`pytest`). Hammasini, bitta guruhni yoki bitta testni ishga tushirish,
holat belgilari (✓ o'tdi, ✕ yiqildi, ○ o'tkazib yuborildi), yiqilgan testning chiqishi, faqat yiqilganlarni qayta
ishga tushirish (↻ Failed), filtr. Testni ikki marta bossangiz — kodi ochiladi. *Test: Run Test at Cursor* —
kursor turgan test funksiyasini ishga tushiradi.

**Testni debug qilish:** test yonidagi **▷** (yoki o'ng tugma → *Debug Test*, *Test: Debug Test at Cursor*) —
faqat shu test debugger ostida ishga tushadi va testdagi breakpoint'da to'xtaydi. Rust testlari gdb / lldb-dap
bilan (test binari avval qayta yig'iladi), pytest testlari debugpy bilan (`python -m pytest <test>`).

![Testing](docs/testing.png)

## Debugger

**Run and Debug** paneli (`Ctrl+Shift+D`, ▷ belgisi) Debug Adapter Protocol orqali ishlaydi:
**Rust / C / C++** — GDB 14+ (`gdb -i dap`) yoki `lldb-dap`, **Python** — `debugpy` (`pip install debugpy`).
Cargo loyihasining binarlari va ochiq Python fayli avtomatik topiladi; boshqalari uchun `.zenith/launch.toml`
(⚙ tugmasi namunani yaratadi):

```toml
[[configurations]]
name = "Debug program"
adapter = "gdb"            # gdb | lldb | debugpy, yoki command = ["boshqa-adapter"]
program = "target/debug/my-program"
args = []
build = "cargo build"      # ishga tushirishdan oldin
```

Breakpoint — qator raqamining chap tomonini bosing yoki `F9` (sessiyada saqlanadi, tahrirda qator bilan siljiydi).
`F5` boshlash / davom ettirish, `F10` step over, `F11` step into, `Shift+F11` step out, `F6` pauza, `Shift+F5` to'xtatish.
To'xtaganda: joriy qator belgilanadi, **o'zgaruvchilar** daraxti (ichiga kirish mumkin), **call stack** (bosib kadrni
tanlash), **debug console** — dastur chiqishi va ifodalarni hisoblash.
Breakpoint ustida o'ng tugma: **shart** (`t.baho == 3` bo'lganda to'xtash), **logpoint** (to'xtamasdan
`{ifoda}` bilan xabar chiqarish, ◆ belgisi), **hit count**, o'chirib qo'yish. **WATCH** bo'limi — har to'xtashda
hisoblanadigan ifodalar; to'xtab turganda o'zgaruvchi ustiga sichqonchani olib borsangiz qiymati ko'rinadi.

![Debugger](docs/debugger.png)

## MCP serverlari

AI Agents panelidagi **MCP SERVERS** bo'limi ish maydonidagi `.mcp.json` faylini tahrirlaydi (Claude Code va
boshqa agentlar MCP serverlarini shu yerdan o'qiydi): qo'shish (tayyor variantlar: filesystem, fetch, git, memory),
tahrirlash, o'chirish. Mahalliy (stdio) yoki masofaviy (HTTP URL) serverlar; muhit o'zgaruvchilari. **Test** —
serverni ishga tushirib, `initialize` + `tools/list` bilan uning vositalarini ko'rsatadi. Ixtiyoriy ravishda
Gemini CLI uchun `.gemini/settings.json` ga ham yoziladi. Fayldagi boshqa sozlamalar saqlanib qoladi.

![MCP serverlari](docs/mcp.png)

## AI agent o'zgarishlarini ko'rib chiqish

Agent ishga tushganda Zenith ish maydonining "checkpoint"ini oladi (Git'ning vaqtinchalik indeksi orqali — sizning
staging'ingizga tegmaydi). AI panelidagi **⧉ Changes** yoki *AI: Review All Agent Changes* buyrug'i shundan beri
o'zgargan barcha fayllarni ko'rsatadi; o'chirilgan fayllarni ham qaytarish mumkin.

![Agent changes](docs/agent-changes.png)

## Git

![Git](docs/git.png)

Source Control'da faylni bossangiz, u HEAD bilan **yonma-yon diff** ko'rinishida ochiladi (Prev/Next bilan o'zgarishlar
orasida yurish, "Open file" — tahrirga qaytish). Gutter'dagi rangli o'zgarish chizig'ini bossangiz — eski/yangi qatorlar, **+ Stage** / **− Unstage** (faylning faqat shu o'zgarishini commit'ga tayyorlash yoki undan chiqarish) va **↺ Revert** (faqat shu o'zgarishni qaytarish). Yonma-yon diff'da ham har o'zgarish yonida **+ Stage** va **↺ Revert** bor; *Git: Stage / Unstage Change at Cursor* buyruqlari ham shuni qiladi. Kursor turgan qator oxirida **git blame** (muallif, vaqt, commit xabari). Status qatoridagi branch nomini bossangiz — **branch almashtirish yoki yangi branch yaratish**.

![Git diff](docs/git-diff.png)

**Merge konfliktlari:** `git merge` / `rebase` / `cherry-pick` to'qnashuvga uchrasa, faylda `<<<<<<<` … `>>>>>>>` bloklari
rangli ko'rinadi (joriy — yashil, kelayotgan — binafsha) va har birida **Accept Current / Accept Incoming / Accept Both**
tugmalari chiqadi; yuqoridagi panelda konfliktlar soni, **Next ↓** va hammasini birdan qabul qilish. Source Control'da
"MERGE CHANGES" bo'limi, **+** bilan "hal qilindi" deb belgilash va **Continue / Abort**. **⊟ Stash / ⊞ Pop** — o'zgarishlarni
vaqtincha yashirish va qaytarish.

![Merge konfliktlari](docs/merge.png)

Tizimdagi `git` buyrug'idan foydalanadi (alohida kutubxona shart emas). Fon holati `--no-optional-locks` bilan o'qiladi,
shuning uchun terminal yoki AI agent bilan bir vaqtda ishlaganda `index.lock` to'qnashuvi bo'lmaydi.

## Sozlamalar

`Ctrl+,` (macOS: `⌘,`) — sozlamalar oynasi: tema, shrift o'lchamlari, tab, avtomatik saqlash, saqlashda formatlash,
standart AI agent, terminal shell va boshqalar. O'zgarishlar darhol qo'llanadi va `settings.toml`ga yoziladi
(maxsus agentlar va til serverlari uchun faylni "Edit settings.toml" tugmasi bilan ochish mumkin).

![Settings](docs/settings.png)

**Loyiha sozlamalari.** `.zenith/settings.toml` (*Preferences: Open Workspace Settings*) — faqat shu papka ochiq
bo'lganda sizning sozlamalaringiz ustidan qo'llanadi (masalan, `tab_size = 2`, `format_on_save = true`); faylni
commit qilsangiz, jamoa ham shu sozlamalarni oladi. Xavfsizlik uchun dastur ishga tushiradigan yoki tarmoqqa
chiqadigan kalitlar (`shell`, `agents`, `language_servers`, sync, registry) bu yerda e'tiborga olinmaydi.
Sozlamalar oynasi qaysi kalitlar loyiha tomonidan almashtirilganini ko'rsatadi.

## Sozlamalarni sinxronlash

Sozlamalar → Advanced → **Settings Sync**: o'zingizning Git repo'ingiz manzilini kiriting (masalan, maxfiy GitHub
repo) va **Connect**. **⬆ Push** — `settings.toml`, `keybindings.toml` va `plugins/` ni repo'ga yuklaydi;
boshqa kompyuterda **⬇ Pull** — ularni oladi (oldingi fayllar `sync-backup/` ga saqlanadi), sozlamalar, tugmalar va
pluginlar darhol qayta yuklanadi. Push hech narsani o'chirmaydi; boshqa kompyuterdagi o'zgarishlar bilan to'qnashsa,
avval Pull qilishni so'raydi. "Pull on startup" — har ishga tushganda olish. Palitrada: *Settings Sync: Push / Pull*.

## Uzoqdan ishlash (SSH)

*File → Open Remote Folder (SSH)…* (palitrada *Remote: Open Folder over SSH*) — manzilni kiriting:
`user@host:/path/to/project`, `host:~/project` yoki `ssh://user@host:2222/path`. `~/.ssh/config` dagi
taxalluslar ham ishlaydi. Tizimdagi `ssh` ishlatiladi, shuning uchun kalit yoki ssh-agent orqali parolsiz kirish
sozlangan bo'lishi kerak (Windows 10+ va macOS/Linux'da `ssh` bor; serverda `sh`, `tar` va `find` kifoya).

- Papka mahalliy nusxaga (`<config>/remote/<host>/…`) yuklanadi — tahrirlash, qidirish, LSP va tree-sitter
  tarmoqni kutmaydi. **Saqlash**, yangi fayl/papka, nomini o'zgartirish va o'chirish darhol serverga yuboriladi.
- **Til serverlari (LSP) va debugger ham serverda** ishlaydi: Zenith serverda qaysi dasturlar borligini tekshiradi
  (`rust-analyzer`, `pyright`, `gopls`, `gdb`, `python3 -m debugpy` …, PATH va `~/.cargo/bin`, `~/.local/bin`, `~/go/bin`
  dan) va ularni `ssh` orqali ishga tushiradi; fayl yo'llari mahalliy nusxa ↔ server o'rtasida avtomatik
  almashtiriladi. Debug konfiguratsiyasidagi `build` buyrug'i ham serverda bajariladi.
- **Terminal va AI agentlar serverda** ishga tushadi (`ssh -t host 'cd papka && …'`) — `cargo`, `npm`, `claude`
  va boshqalar serverdagi muhitda ishlaydi.
- Serverdagi o'zgarishlar (agent yoki terminal qilgan) har 4 soniyada olinadi; ochiq fayl o'zgarsa, odatdagi diff
  banneri chiqadi (**Keep / Revert** — Revert ham serverga yoziladi).
- Status qatorida **SSH host**: bossangiz — hoziroq yangilaydi; ulanish uzilsa qizil "(offline)" ko'rinadi.
- **Source Control to'liq ishlaydi** — stage, commit, push/pull, branch, stash, merge konfliktlari: `git` buyruqlari
  serverning o'zida bajariladi (serverdagi git sozlamalari va kalitlari bilan). Git fayllarni o'zgartirsa
  (pull, checkout, stash), ular darhol mahalliy nusxaga olinadi; serverda o'chirilgan fayllar nusxadan ham o'chadi.
- Oxirgi ulanishlar eslab qolinadi; Zenith qayta ochilganda uzoq papka avtomatik qayta ulanadi.

## Klaviatura

**O'z tugmalaringiz:** *Preferences: Keyboard Shortcuts (TOML)* (yoki Sozlamalar → Editor → "Edit keybindings.toml")
`keybindings.toml`ni ochadi — unda barcha buyruqlar ID'lari ro'yxati bor. Saqlash bilan darhol qo'llanadi,
menyu va palitradagi yozuvlar ham yangilanadi:

```toml
[bindings]
"Ctrl+Shift+D" = "split_editor"          # buyruqqa tugma
"Alt+T" = "plugin:todo-tree.ask-ai"      # plugin buyrug'iga tugma
"Ctrl+B" = ""                            # standart tugmani o'chirish
```

`Mod` — Linux/Windows'da Ctrl, macOS'da ⌘. Pluginlar ham `plugin.toml`da `keybinding = "..."` bera oladi.

**Vim rejimi** (Sozlamalar → Editor → Vim mode yoki *Preferences: Toggle Vim Mode*): Normal / Insert / Visual /
Visual Line; `h j k l w b e W B E 0 ^ $ gg G f t F T { }`, sonlar (`3j`, `d2w`), operatorlar `d c y > <` harakatlar va
matn obyektlari bilan (`ciw`, `da(`, `ci"`, `yi{`), `dd yy cc x X D C s S r ~ J p P u Ctrl+R o O i a I A v V`,
`/ n N *`, `.` (oxirgi o'zgarishni takrorlash), makroslar `q<r>` … `q`, `@<r>`, `@@`, almashtirish
`:s/eski/yangi/`, `:%s/…/…/gi`, `:2,5s/…`, `:'<,'>s/…` (`\1`, `&` guruhlar), `:w :q :wq :x :<qator>`. Rejim status qatorida, Normal rejimda blok kursor.

| Amal | Windows / Linux | macOS |
|---|---|---|
| Buyruqlar palitrasi | `Ctrl+Shift+P` / `F1` | `⌘⇧P` |
| Faylni tez ochish | `Ctrl+P` | `⌘P` |
| Papka ochish | `Ctrl+Shift+O` | `⌘⇧O` |
| Ish maydoniga yana papka qo'shish | File → Add Folder to Workspace | File → Add Folder to Workspace |
| Saqlash | `Ctrl+S` | `⌘S` |
| Qidirish / Almashtirish | `Ctrl+F` / `Ctrl+H` | `⌘F` / `⌘⌥F` |
| Fayllarda qidirish | `Ctrl+Shift+F` | `⌘⇧F` |
| Qatorga o'tish | `Ctrl+G` | `⌘G` |
| Terminal | ``Ctrl+` `` | ``⌃` `` |
| AI panel | `Ctrl+Shift+A` | `⌘⇧A` |
| AI'dan so'rash (belgilangan kod) | `Ctrl+Shift+I` | `⌘⇧I` |
| Belgilangan kodni agentga yuborish | `Ctrl+Shift+L` | `⌘⇧L` |
| Avtomatik to'ldirish | `Ctrl+Space` | `⌃Space` |
| Ta'rifga o'tish | `F12` / `Ctrl+bosish` | `F12` / `⌘+bosish` |
| Keyingi xato | `F8` | `F8` |
| So'zlarni o'rash (word wrap) | `Alt+Z` | `⌥Z` |
| Blokni yig'ish / ochish | `Ctrl+Shift+[` / `Ctrl+Shift+]` | `⌘⇧[` / `⌘⇧]` |
| Havolalarni topish | `Shift+F12` | `⇧F12` |
| Nomni o'zgartirish (rename) | `F2` | `F2` |
| Formatlash | `Shift+Alt+F` | `⇧⌥F` |
| Tezkor tuzatish / kod amallari | `Ctrl+.` | `⌘.` |
| Fayldagi belgiga o'tish | `Ctrl+T` | `⌘T` |
| Loyihadagi belgiga o'tish | `Ctrl+Shift+T` | `⌘⇧T` |
| Parametr maslahatlari | `Ctrl+Shift+Space` | `⌘⇧Space` |
| Izoh qo'yish/olib tashlash | `Ctrl+/` | `⌘/` |
| Qatorni ko'chirish / nusxalash | `Alt+↑↓` / `Alt+Shift+↑↓` | `⌥↑↓` / `⌥⇧↑↓` |
| Keyingi moslikka kursor qo'shish | `Ctrl+D` | `⌘D` |
| Barcha mosliklarni belgilash | `Ctrl+F2` | `⌘F2` |
| Yuqori/pastga kursor qo'shish | `Ctrl+Alt+↑↓` | `⌘⌥↑↓` |
| Kursor qo'shish (sichqoncha) | `Alt+bosish` | `⌥+bosish` |
| Ustun bo'yicha belgilash | `Shift+Alt+sudrash` | `⇧⌥+sudrash` |
| Qatorni o'chirish | `Ctrl+Shift+K` | `⌘⇧K` |
| Source Control | `Ctrl+Shift+G` | `⌃⇧G` |
| Problems (barcha xatolar) | `Ctrl+Shift+M` | `⌘⇧M` |
| Split editor (yonma-yon) | `Ctrl+\` | `⌘\` |
| Boshqa panelga o'tish | `Ctrl+Alt+\` | `⌃⌥\` |
| Yon panel | `Ctrl+B` | `⌘B` |
| Masshtab | `Ctrl+=` / `Ctrl+-` / `Ctrl+0` | `⌘=` / `⌘-` / `⌘0` |

Terminal fokusda bo'lganda (Windows/Linux) oddiy `Ctrl+harf` kombinatsiyalari shell'ga uzatiladi (`Ctrl+C`, `Ctrl+R`…); global buyruqlar `Ctrl+Shift+…` orqali ishlaydi.

## Arxitektura

```
crates/
├── zenith-core      Matn buferi (ropey), kursor/belgilash, undo/redo, qidiruv — UI'ga bog'liq emas, testlangan
├── zenith-terminal  PTY + VT100 emulyatori (portable-pty, vt100), terminal so'rovlariga javob
├── zenith-plugin    Plugin xosti: manifest, JSON-lines protokoli, fon oqimlari
├── zenith-lsp       Language Server Protocol mijozi (JSON-RPC, UTF-16 pozitsiyalar, snippetlar)
├── zenith-dap       Debug Adapter Protocol mijozi (gdb, lldb-dap, debugpy), launch.toml
├── zenith-ai        AI agent profillari (Claude Code, Codex, Gemini, Aider…), prompt yasash
└── zenith-app       GUI (eframe/egui): muharrir vidjeti, panellar, palitra, temalar
plugins/examples/    Namuna pluginlar (Python): text-tools, todo-tree (yon panel)
```

Batafsil: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Rivojlantirish

```bash
cargo test --workspace       # barcha testlar
cargo clippy --workspace     # lint
cargo fmt --all              # formatlash
```

Sozlamalar joyi: Linux `~/.config/zenith/`, macOS `~/Library/Application Support/dev.Zenith.zenith/`,
Windows `%APPDATA%\Zenith\zenith\config\` (`ZENITH_CONFIG_DIR` bilan o'zgartirish mumkin).

## Litsenziya

Apache-2.0. Shriftlar: JetBrains Mono va Inter — SIL Open Font License (`assets/fonts/`).
