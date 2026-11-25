# Zenith plugin qo'llanmasi

Zenith plugini — bu **istalgan tilda yozilgan oddiy dastur**. Editor uni alohida jarayon sifatida
ishga tushiradi va u bilan stdin/stdout orqali **bir qatorda bitta JSON** (JSON Lines) almashadi.

Afzalliklari:
- Har qanday til: Python, Node.js, Go, Rust, Bash…
- Xavfsiz va barqaror: plugin sekin ishlasa yoki yiqilsa ham editor qotmaydi.
- Hech qanday SDK shart emas — JSON o'qish/yozish kifoya.

## Plugin qayerda turadi

```
<config>/plugins/<plugin-papkasi>/plugin.toml     # barcha loyihalar uchun
<workspace>/.zenith/plugins/<plugin-papkasi>/      # faqat shu loyiha uchun
```

`<config>` — sozlamalar papkasi (Plugins paneli → **Folder** tugmasi uni ochadi).
**+ Create sample** tugmasi tayyor namunani yaratadi: [`plugins/examples/text-tools`](../plugins/examples/text-tools).

## Manifest: `plugin.toml`

```toml
id = "text-tools"                 # yagona ID
name = "Text Tools"
version = "0.1.0"
description = "Matn bilan ishlash vositalari"
run = ["python3", "main.py"]      # ishga tushirish buyrug'i (nisbiy yo'llar plugin papkasidan)
events = ["file_saved"]           # ixtiyoriy: "file_opened", "file_saved", "active_editor_changed"

[[commands]]
id = "text-tools.word-count"      # buyruq ID
title = "Text Tools: Word Count"  # palitrada ko'rinadigan nom
```

Buyruqlar avtomatik ravishda **Command Palette**, **Plugins** menyusi va Plugins panelida paydo bo'ladi.
Plugin birinchi buyruq chaqirilganda ishga tushadi (lazy).

## Protokol (v1)

### Editor → plugin

```json
{"id":0,"method":"initialize","params":{"protocol":1,"editor":"zenith","version":"0.1.0"}}
{"id":1,"method":"execute","params":{"command":"text-tools.word-count","context":{...}}}
{"id":2,"method":"event","params":{"event":"file_saved","data":{"path":"/abs/path"}}}
{"method":"shutdown"}
```

`context` maydonlari:

| Maydon | Turi | Tavsif |
|---|---|---|
| `workspace` | string? | Ochiq papka |
| `file` | string? | Aktiv fayl yo'li |
| `language` | string? | Til nomi (masalan `Rust`) |
| `text` | string? | Faylning to'liq matni |
| `selection` | string | Belgilangan matn |
| `cursor_line`, `cursor_col` | number | Kursor (0 dan) |

### Plugin → editor

Javob (`id` so'rovniki bilan bir xil):

```json
{"id":1,"result":{"actions":[{"type":"notify","message":"42 so'z"}]}}
{"id":1,"error":"nimadir xato ketdi"}
```

Istalgan vaqtda (so'rovsiz) ham amal yuborish mumkin:

```json
{"method":"action","params":{"type":"open_file","path":"src/main.rs","line":10}}
{"method":"log","params":{"message":"debug ma'lumot"}}
```

stderr'ga yozilgan hamma narsa log hisoblanadi.

### Amallar (actions)

| `type` | Maydonlar | Natija |
|---|---|---|
| `notify` | `message`, `level`: `info`/`warning`/`error` | Bildirishnoma ko'rsatadi |
| `replace_selection` | `text` | Belgilangan matnni almashtiradi |
| `insert` | `text` | Kursor joyiga qo'shadi |
| `set_text` | `text` | Butun faylni almashtiradi (bitta undo qadami) |
| `open_file` | `path`, `line`? | Faylni ochadi (nisbiy yo'l = workspace'dan) |
| `run_in_terminal` | `command` | Yangi terminalda buyruqni bajaradi |
| `send_to_agent` | `text`, `agent`? | AI agentga matn yuboradi (kerak bo'lsa ishga tushiradi) |
| `set_panel` | `panel`, `items`, `message`?, `buttons`? | Plugin panelining tarkibini almashtiradi (pastga qarang) |
| `write_file` | `path`, `text` | Faylni yozadi (nisbiy = workspace'dan). WASM: `write_workspace` ruxsati va tasdiq kerak |
| `http_request` | `request_id`, `url`, `method`?, `headers`?, `body`? | Editor so'rovni bajaradi va pluginga `{"method":"http_response","params":{"request_id":…,"status":200,"body":"…"}}` (yoki `"error"`) yuboradi. WASM: faqat `network:<host>` dagi hostlar |
| `set_status` | `id`, `text`, `tooltip`?, `command`?, `align`? (`left`/`right`), `color`? | Status qatoriga element qo'shadi/yangilaydi; bo'sh `text` — o'chiradi. Bosilganda `command` chaqiriladi |

## Yon panel (sidebar panel)

Plugin activity bar'da o'z ikonkasi bo'lgan panel qo'shishi mumkin:

```toml
[[panels]]
id = "todos"
title = "TODO Tree"
icon = "☑"          # bitta belgi; bo'sh bo'lsa sarlavhaning birinchi harfi
```

Foydalanuvchi panelni ochganda (yoki ⟳ bosganda) plugin ishga tushadi va quyidagi hodisani oladi:

```json
{"id":3,"method":"event","params":{"event":"panel_opened","data":{"panel":"todos","context":{...}}}}
```

Plugin panel tarkibini `set_panel` amali bilan yuboradi — javob sifatida yoki istalgan vaqtda (masalan, `file_saved` dan keyin):

```json
{"method":"action","params":{"type":"set_panel","panel":"todos",
  "message":"3 ta vazifa",
  "buttons":[{"title":"✳ Ask AI","command":"todo-tree.ask-ai"}],
  "items":[
    {"label":"main.rs","detail":"src","header":true},
    {"icon":"○","label":"handle empty names","detail":"L9","indent":1,"path":"src/main.rs","line":9},
    {"label":"Qayta ishlash","command":"my.cmd","id":"item-7"}
  ]}}
```

`items` maydonlari: `label`, `detail` (xira matn), `icon`, `header` (bo'lim sarlavhasi), `indent`, `tooltip`;
bosilganda `path` + `line` faylni ochadi, yoki `command` plugin buyrug'ini chaqiradi (`execute` so'rovida
`"item": <id>` bo'ladi). `buttons` — panel sarlavhasidagi tugmalar.

To'liq misol: [`plugins/examples/todo-tree`](../plugins/examples/todo-tree/main.py).

![Plugin paneli](plugin-panel.png)

## Eng kichik plugin (Node.js)

```js
// main.js — run = ["node", "main.js"]
const rl = require("readline").createInterface({ input: process.stdin });
rl.on("line", (line) => {
  const msg = JSON.parse(line);
  if (msg.method === "execute") {
    const sel = msg.params.context.selection;
    const actions = [{ type: "replace_selection", text: sel.split("").reverse().join("") }];
    process.stdout.write(JSON.stringify({ id: msg.id, result: { actions } }) + "\n");
  }
});
```

## Status qatori

Plugin status qatoriga o'z elementini qo'yishi mumkin (masalan, so'zlar soni, build holati):

```json
{"method":"action","params":{"type":"set_status","id":"words","text":"42 words","align":"right","command":"text-tools.word-count"}}
```

`active_editor_changed` hodisasi (`{"path": ...}`) aktiv fayl almashganda keladi. Misol:
[`plugins/examples/text-tools`](../plugins/examples/text-tools/main.py).

## WebAssembly (WASM) pluginlar

`run` o'rniga `wasm` ko'rsatilsa, plugin **WebAssembly** moduli sifatida Zenith ichida (wasmtime) ishlaydi:

```toml
id = "case-converter"
name = "Case Converter (WASM)"
wasm = "case_converter.wasm"   # WASI command modul (wasm32-wasip1)
```

- Protokol xuddi shu: stdin'dan JSON qatorlar keladi, stdout'ga javob yoziladi, stderr — log.
- **Sandbox**: modul faqat editor yuborgan xabarlarni ko'radi — fayllar, tarmoq, muhit o'zgaruvchilari yo'q.
  Kerakli ma'lumot (`text`, `selection`, ...) `context` da keladi, o'zgarishlar amallar (`replace_selection`,
  `set_text`, ...) orqali qilinadi. Python yoki Node o'rnatilishi shart emas — bitta `.wasm` fayl hamma OS'da ishlaydi.
- **Ruxsatlar** (`permissions = [...]`):

  | Ruxsat | Nima beradi |
  |---|---|
  | `read_workspace` | Ochiq papka `/workspace` da, faqat o'qish uchun |
  | `write_workspace` | `write_file` amali bilan fayl yozishni **so'rash** — har safar foydalanuvchi tasdiqlaydi (Allow / Always allow / Deny), faqat workspace ichida |
  | `network:<host>` | `http_request` amali bilan faqat shu hostga HTTP so'rov (so'rovni editor bajaradi; modul ichida tarmoq yo'q) |

  `read_workspace`: plugin ochiq papkani `/workspace` sifatida **faqat o'qiy oladi**
  (yozish, yaratish, o'chirish rad etiladi). `context` dagi va javobdagi yo'llar avtomatik almashtiriladi
  (`/workspace/src/a.rs` ↔ haqiqiy yo'l). Plugins panelida "WASM · reads workspace" deb ko'rinadi.
  Misol: [`plugins/examples/workspace-stats`](../plugins/examples/workspace-stats).
- Plugin o'chirilganda yoki qotib qolsa, Zenith uni darhol to'xtatadi.
- Istalgan WASI'ni qo'llaydigan til: Rust (`cargo build --target wasm32-wasip1`), Go (`GOOS=wasip1 GOARCH=wasm`),
  C/C++ (wasi-sdk), Zig va boshqalar.

To'liq misol: [`plugins/examples/case-converter`](../plugins/examples/case-converter) (Rust; `./build.sh` bilan yig'iladi).

## Marketplace: pluginni e'lon qilish

Plugins panelidagi **MARKETPLACE** bo'limi plugin indeksini o'qiydi — standart holatda shu repodagi
[`registry/plugins.json`](../registry/plugins.json). **Install** pluginni Git repo'dan klonlaydi (plugin katta repo
ichidagi papka bo'lsa — faqat o'sha papka, sparse checkout bilan) va `plugins/<id>/` ga qo'yadi; indeksdagi versiya
o'rnatilgandan yangi bo'lsa **Update** chiqadi; **Uninstall** papkani o'chiradi.

O'z pluginingizni qo'shish uchun `registry/plugins.json` ga yozuv qo'shib pull request oching:

```json
{
  "id": "my-plugin",
  "name": "My Plugin",
  "version": "0.1.0",
  "description": "Bir qatorda nima qilishi.",
  "author": "Ismingiz",
  "repository": "https://github.com/you/my-plugin.git",
  "path": "",
  "rev": "v0.1.0",
  "tags": ["git", "ai"]
}
```

- `id` — `plugin.toml` dagi `id` bilan bir xil bo'lishi shart (aks holda o'rnatish rad etiladi).
- `path` — plugin repo ichidagi papkada bo'lsa (bo'sh = repo ildizi).
- `rev` — o'rnatiladigan tag yoki branch (bo'sh = asosiy branch). Yangi versiya chiqarganda `version` va `rev` ni yangilang.

Kompaniya ichidagi o'z indeksingiz: Sozlamalar → Advanced → **Plugin marketplace** — URL yoki fayl yo'li.
