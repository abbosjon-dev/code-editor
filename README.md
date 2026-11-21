# ◆ Zenith

**Fast. Extensible. AI-native.** — Rust tilida yozilgan, VS Code uslubidagi kod muharriri.
Claude Code, Codex, Gemini CLI, Aider kabi AI agentlar to'g'ridan-to'g'ri editor ichida ishlaydi.
macOS, Windows va Linux'da ishlaydi.

![Zenith screenshot](docs/screenshot.png)

## Imkoniyatlar

| | |
|---|---|
| ⚡ **Tezlik** | Rust + GPU orqali chizish (egui/OpenGL). Faqat ko'rinadigan qatorlar chiziladi; sintaksis ranglash inkremental va har kadrda vaqt bilan cheklangan — katta fayllarda ham UI qotmaydi. |
| ✳ **AI agentlar** | O'ng tomondagi AI panelda haqiqiy agent CLI'lari ishlaydi: **Claude Code**, OpenAI **Codex**, **Gemini CLI**, **Aider**, **OpenCode** yoki o'zingizning har qanday CLI'ingiz. "Ask AI" (`Ctrl+Shift+I`) belgilangan kodni fayl yo'li bilan agentga yuboradi. Agent faylni o'zgartirsa, editor uni darhol yuklaydi va **o'zgarishlarni diff ko'rinishida** ko'rsatadi — har birini qoldirish yoki bitta tugma bilan qaytarish mumkin. |
| 🧩 **Pluginlar** | Plugin — istalgan tildagi dastur (Python, JS, Go, Rust…). Alohida jarayonda ishlaydi, shuning uchun plugin xatosi editorni hech qachon qotirmaydi yoki buzmaydi. [Qo'llanma →](docs/PLUGINS.md) |
| ⌨ **Terminal** | To'liq terminal emulyatori (PTY: Unix'da openpty, Windows'da ConPTY), 256/truecolor, scrollback, sichqoncha, nusxa olish, bir nechta tab. `Ctrl+bosish` bilan chiqishdagi `fayl:qator:ustun` havolalarini muharrirda va URL'larni brauzerda ochadi. |
| 📝 **Muharrir** | **Multi-cursor** (Ctrl+D, Alt+bosish, Ctrl+Alt+↑↓), **split editor** (`Ctrl+\`), **kod bloklarini yig'ish** (gutter'dagi ▾ yoki `Ctrl+Shift+[` / `]`), sintaksis ranglash (~200 til), avtomatik qavs yopish, aqlli indent, bir nechta tab, undo/redo, qidirish va almashtirish, minimap, indent chiziqlari, qavs juftini ko'rsatish, qatorni ko'chirish/nusxalash, izoh qo'yish. |
| 🧠 **LSP (til serverlari)** | Avtomatik to'ldirish, xatolar va ogohlantirishlar (qizil to'lqinli chiziq), hover hujjatlari, ta'rifga o'tish (`F12` / `Ctrl+bosish`), havolalarni topish (`Shift+F12`), nomni hamma joyda o'zgartirish (`F2`), formatlash (`Shift+Alt+F`, ixtiyoriy saqlashda formatlash), parametr maslahatlari, tezkor tuzatishlar (`Ctrl+.` / 💡), fayldagi belgiga o'tish (`Ctrl+T`). O'rnatilgan bo'lsa avtomatik ishga tushadi: **rust-analyzer**, **pyright**, **typescript-language-server**, **gopls**, **clangd** va boshqalar. |
| ⎇ **Git** | Gutter'da o'zgarish belgilari (qo'shilgan / o'zgargan / o'chirilgan), Source Control paneli (`Ctrl+Shift+G`): stage/unstage, commit (`Ctrl+Enter`), push/pull, fayl daraxtida rangli holat. **✳ Commit with AI** — agent diff'ni ko'rib, commit'ni o'zi yozadi. |
| 🔍 **Navigatsiya** | Buyruqlar palitrasi (`Ctrl+Shift+P`), fuzzy fayl qidirish (`Ctrl+P`), fayllar ichida qidirish (`Ctrl+Shift+F`, `.gitignore`'ni hisobga oladi), qatorga o'tish. |
| 🎨 **Chiroyli UI** | Zenith Midnight (qorong'i) va Daylight (yorug') temalari, 30+ sintaksis temasi, JetBrains Mono + Inter shriftlari. |

## Yuklab olish

[Releases](https://github.com/abbosjon-dev/code-editor/releases) sahifasidan:

| OS | Fayl | O'rnatish |
|---|---|---|
| **macOS** (Apple Silicon / Intel) | `Zenith-…-macos-arm64.dmg` / `…-x86_64.dmg` | DMG'ni oching, Zenith'ni Applications'ga torting. Ilova imzolanmagan: birinchi marta **o'ng tugma → Open** (yoki `xattr -dr com.apple.quarantine /Applications/Zenith.app`). |
| **Windows** | `zenith-…-windows-x86_64.zip` | Arxivni oching va `zenith.exe` ni ishga tushiring. |
| **Ubuntu / Debian** | `zenith_…_amd64.deb` | `sudo apt install ./zenith_…_amd64.deb` |
| **Boshqa Linux** | `zenith-…-linux-x86_64.tar.gz` | Arxivni oching va `./install.sh` (foydalanuvchi uchun, `~/.local`ga). |

Paketlarni o'zingiz yig'ish: `bash packaging/package.sh` (natija `dist/` papkasida).

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
3. Kodni belgilang va `Ctrl+Shift+I` bosing — savolingiz kod bilan birga agentga yuboriladi.
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

## Git

![Git](docs/git.png)

Tizimdagi `git` buyrug'idan foydalanadi (alohida kutubxona shart emas). Fon holati `--no-optional-locks` bilan o'qiladi,
shuning uchun terminal yoki AI agent bilan bir vaqtda ishlaganda `index.lock` to'qnashuvi bo'lmaydi.

## Sozlamalar

`Ctrl+,` (macOS: `⌘,`) — sozlamalar oynasi: tema, shrift o'lchamlari, tab, avtomatik saqlash, saqlashda formatlash,
standart AI agent, terminal shell va boshqalar. O'zgarishlar darhol qo'llanadi va `settings.toml`ga yoziladi
(maxsus agentlar va til serverlari uchun faylni "Edit settings.toml" tugmasi bilan ochish mumkin).

![Settings](docs/settings.png)

## Klaviatura

| Amal | Windows / Linux | macOS |
|---|---|---|
| Buyruqlar palitrasi | `Ctrl+Shift+P` / `F1` | `⌘⇧P` |
| Faylni tez ochish | `Ctrl+P` | `⌘P` |
| Papka ochish | `Ctrl+Shift+O` | `⌘⇧O` |
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
| Blokni yig'ish / ochish | `Ctrl+Shift+[` / `Ctrl+Shift+]` | `⌘⇧[` / `⌘⇧]` |
| Havolalarni topish | `Shift+F12` | `⇧F12` |
| Nomni o'zgartirish (rename) | `F2` | `F2` |
| Formatlash | `Shift+Alt+F` | `⇧⌥F` |
| Tezkor tuzatish / kod amallari | `Ctrl+.` | `⌘.` |
| Fayldagi belgiga o'tish | `Ctrl+T` | `⌘T` |
| Parametr maslahatlari | `Ctrl+Shift+Space` | `⌘⇧Space` |
| Izoh qo'yish/olib tashlash | `Ctrl+/` | `⌘/` |
| Qatorni ko'chirish / nusxalash | `Alt+↑↓` / `Alt+Shift+↑↓` | `⌥↑↓` / `⌥⇧↑↓` |
| Keyingi moslikka kursor qo'shish | `Ctrl+D` | `⌘D` |
| Barcha mosliklarni belgilash | `Ctrl+F2` | `⌘F2` |
| Yuqori/pastga kursor qo'shish | `Ctrl+Alt+↑↓` | `⌘⌥↑↓` |
| Kursor qo'shish (sichqoncha) | `Alt+bosish` | `⌥+bosish` |
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
├── zenith-ai        AI agent profillari (Claude Code, Codex, Gemini, Aider…), prompt yasash
└── zenith-app       GUI (eframe/egui): muharrir vidjeti, panellar, palitra, temalar
plugins/examples/    Namuna plugin (Python)
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
