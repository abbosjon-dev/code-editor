# Arxitektura va reja

## Tamoyillar

1. **UI hech qachon kutmaydi.** Fayl indekslash, fayllarda qidirish, terminal o'qish, pluginlar — barchasi
   fon oqimlarida. UI har kadrda kanallardan natijalarni "so'raydi" (poll) va faqat kerak bo'lganda qayta chiziladi.
2. **Faqat ko'rinadigan narsa chiziladi.** Muharrir faqat ekrandagi qatorlarni joylashtiradi (virtualizatsiya).
3. **Inkremental ishlov.** Sintaksis holati har qator boshida saqlanadi; tahrirdan keyin faqat o'zgargan qatordan
   pastga, faqat ekran talab qilgancha qayta hisoblanadi. Har kadrda ~6 ms limit.
4. **Izolyatsiya.** Pluginlar va AI agentlar alohida jarayonlarda — ular yiqilsa ham editor ishlayveradi.
5. **Yadro UI'dan mustaqil.** `zenith-core` toza mantiq, unit testlar bilan qoplangan.

## Crate'lar

| Crate | Mas'uliyat | Asosiy bog'liqliklar |
|---|---|---|
| `zenith-core` | `Buffer` (rope), `Selection`, `History` (guruhlangan undo/redo), `find_all` | `ropey` |
| `zenith-terminal` | PTY'da jarayon ishga tushirish, VT100 parse, DSR/DA so'rovlariga javob, scrollback | `portable-pty`, `vt100` |
| `zenith-plugin` | Manifest topish, jarayonni boshqarish, JSON-lines protokoli, WASM pluginlar | `serde_json`, `toml`, `wasmtime` |
| `zenith-lsp` | LSP mijozi: server jarayonlari, Content-Length framing, server so'rovlariga javob, UTF-16 ↔ char | `serde_json`, `url` |
| `zenith-dap` | DAP mijozi: adapter jarayoni, so'rov/javob/hodisalar, adapterlar ro'yxati, `launch.toml` | `serde_json`, `toml` |
| `zenith-ai` | Agent profillari, foydalanuvchi agentlarini birlashtirish, kontekst prompt | `which` |
| `zenith-app` | Oyna, panellar, muharrir vidjeti, terminal ko'rinishi, palitra, temalar | `eframe`, `syntect`, `ignore`, `nucleo-matcher` |

## Ma'lumot oqimi

```
klaviatura/sichqoncha ─► egui ─► ZenithApp::update
                                   ├─ global shortcut'lar ─► Cmd ─► run()
                                   ├─ editor_ui ─► Buffer (core) ─► HighlightCache ─► painter
                                   ├─ terminal_ui ─► Terminal.write() ──► PTY ──► jarayon
                                   │                       ▲ fon oqimi: PTY o'qish ─► vt100 ─► request_repaint
                                   └─ PluginHost.poll() ◄── fon oqimi: plugin stdout ─► Action
```

## Ishlash tezligi (o'lchovlar)

`cargo test --release -p zenith-app perf_workspace -- --ignored --nocapture` (`ZENITH_PERF_DIR` — istalgan katta
papka; standart: cargo registry manbalari). 29 853 fayl, ~780 MB, 4 yadro, disk keshi issiq:

| Amal | Oldin | Hozir |
|---|---|---|
| Fayl indeksi (Quick Open) | 93 ms | 93–117 ms |
| Fuzzy qidiruv (har tugma) | 3 ms | 3 ms |
| Fayllarda qidirish, topilmaydigan so'z (hamma fayl o'qiladi) | 1 380 ms | **147 ms** |
| Fayllarda qidirish, regex `fn\s+new\(` | 830 ms | **95 ms** |
| Fayllarda qidirish, ko'p uchraydigan so'z (5 000 natija) | 84 ms | 14 ms |

**Katta fayl** (120 MB log, 1,34 mln qator; `cargo test --release -p zenith-app perf_huge_log -- --ignored --nocapture`):

| Amal | Oldin | Hozir |
|---|---|---|
| Ochish (`Buffer::load`) | 471 ms | 339 ms |
| Birinchi kadr | 608 ms | 116 ms |
| Bir tugma bosish | ~600 ms (eng uzun qatorni qayta hisoblash) | **6 µs** |
| Fayl ichida qidirish (Ctrl+F) | 1 240 ms | **63 ms** |
| Saqlash | 185 ms | 173 ms |

8 MB dan katta fayllar fonda yuklanadi (tabda "Loading…"; eng uzun qator ham o'sha yerda o'lchanadi) — ochishda UI kadri 100 ms dan oshmaydi. Qidiruv natijalari 100 000 ta bilan cheklanadi ("100000+"), 100 ms dan uzun kadrlar `RUST_LOG=warn` da logga yoziladi.

Nima o'zgardi: fayllar parallel o'qiladi (`ignore` parallel walker); har fayl avval butunligicha bayt-regex bilan
tekshiriladi (mos kelmasa — qatorlarga bo'linmaydi va UTF-8 ga o'girilmaydi); qator raqami faqat topilgan joygacha
sanaladi. Natijalar ro'yxati faqat ko'rinadigan qatorlarni chizadi (oldin har kadrda 5 000 qator va fayl sarlavhasi
uchun O(n²) hisob).

## Yo'l xaritasi (Roadmap)

**v0.1** — muharrir, terminal, AI agent panel, pluginlar, qidiruv, temalar, CI (3 OS).

**v0.2 (hozirgi)** — ✅ LSP: avtomatik to'ldirish, diagnostika, hover, ta'rifga o'tish. ✅ Git: gutter belgilari, Source Control paneli, AI commit. ✅ Multi-cursor. ✅ Split editor. ✅ AI o'zgarishlarini diff bilan ko'rib chiqish va qaytarish.

**v0.2**
- [x] Ustun bo'yicha (blok) belgilash: Shift+Alt+sudrash
- [x] LSP: rename, find references, formatting / format on save
- [x] LSP: signature help, code actions (quick fix), document symbols
- [x] LSP: inlay hints, workspace symbols
- [x] LSP: semantic tokens
- [x] Git: yonma-yon diff ko'rinishi, branch almashtirish
- [x] Git: hunk'ni qaytarish (gutter), inline blame
- [x] Git: stash, merge konfliktlarini hal qilish UI
- [x] Ko'p papkali ish maydoni (har papka o'z Git repo'si va til serveri)
- [x] Debugger (DAP): gdb, lldb-dap, debugpy; breakpoint, step, o'zgaruvchilar, call stack, console
- [x] Sozlanadigan tugmalar (keybindings.toml) va Vim rejimi
- [x] Agent o'zgarishlarini ko'p faylli ko'rib chiqish (checkpoint), terminalni bo'lish
- [x] Shartli breakpoint'lar, logpoint'lar, watch ifodalari, hover qiymatlari

**v0.3**
- [x] Kod bloklarini yig'ish (chekinish asosida, barcha tillar uchun)
- [x] Tree-sitter asosida ranglash va sintaktik folding (11 til)
- [x] MCP serverlarini sozlash UI (.mcp.json, test)
- [x] WASM pluginlar (wasmtime) — sandbox'langan, tez yuklanadigan pluginlar uchun qo'shimcha variant
- [x] Plugin yon panellari (`[[panels]]`, `set_panel`)
- [x] Plugin UI kengaytmalari: status bar elementlari

**v1.0**
- [x] Plugin marketplace / registry (`registry/plugins.json`, Git'dan o'rnatish)
- [x] Paketlar: .dmg (macOS), .zip (Windows), .deb va .tar.gz (Linux)
- [x] .msi (WiX), AppImage, kod imzosi va notarizatsiya (secret'lar bo'lsa), yangilanishni tekshirish
- [x] Yangilanishni yuklab olib o'rnatish (SHA-256 tekshiruvi bilan, foydalanuvchi so'raganda)
- [x] Sozlamalar oynasi
- [x] Sozlamalarni Git repo orqali sinxronlash
- [x] Test paneli (cargo test, pytest), testni debugger ostida ishga tushirish
- [x] SSH orqali uzoqdan ishlash (mahalliy nusxa + serverda terminal/agentlar/git/LSP/debugger)
- [x] Sticky scroll (tree-sitter fold diapazonlari yoki chekinish asosida), Zen rejimi
- [x] Ikki faylni / fayl va clipboard'ni yonma-yon solishtirish
- [x] Avtomatik import (`additionalTextEdits`, `completionItem/resolve`), Organize Imports (LSP + o'rnatilgan saralash)
- [x] Run: loyiha turini aniqlash, `run.toml`, terminal tabida ishga tushirish, URL topish, SSH port-forward
- [x] Web preview (wry: WKWebView / WebView2 / WebKitGTK), qurilma o'lchamlari
- [x] Flutter: qurilmalar, web-server preview, hot reload (saqlaganda), DAP debug
- [x] Expo: Expo Go QR kodi, web preview
- [x] Emulator/simulator ishga tushirish, qurilma ekrani (adb screencap + input; iOS — ko'rish)
- [x] Run chiqishidagi xatolar Problems panelida; Build menyusi (APK/AAB/IPA/web, EAS) va APK o'rnatish
- [x] Bir nechta qurilmada birdaniga ishga tushirish; tezroq qurilma ekrani (raw screencap, scrcpy)
- [x] Compound run'lar, `.env` fayllar (Flutter `--dart-define-from-file`), Flutter DevTools preview'da
- [x] Qurilma loglari (logcat, simulator), Dart exception'lari Problems'da
- [x] HTTP so'rovlar (`.http` fayllar, curl), ma'lumotlar bazasi paneli (sqlite3/psql/mysql), Docker paneli
- [x] Saqlaganda run'ni qayta ishga tushirish (watch rejimi)
