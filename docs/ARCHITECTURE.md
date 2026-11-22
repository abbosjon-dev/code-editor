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
| `zenith-plugin` | Manifest topish, jarayonni boshqarish, JSON-lines protokoli | `serde_json`, `toml` |
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

## Yo'l xaritasi (Roadmap)

**v0.1** — muharrir, terminal, AI agent panel, pluginlar, qidiruv, temalar, CI (3 OS).

**v0.2 (hozirgi)** — ✅ LSP: avtomatik to'ldirish, diagnostika, hover, ta'rifga o'tish. ✅ Git: gutter belgilari, Source Control paneli, AI commit. ✅ Multi-cursor. ✅ Split editor. ✅ AI o'zgarishlarini diff bilan ko'rib chiqish va qaytarish.

**v0.2**
- [ ] Ustun bo'yicha (blok) belgilash: Shift+Alt+sudrash
- [x] LSP: rename, find references, formatting / format on save
- [x] LSP: signature help, code actions (quick fix), document symbols
- [x] LSP: inlay hints, workspace symbols
- [ ] LSP: semantic tokens
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
- [ ] MCP serverlarini sozlash UI
- [ ] WASM pluginlar (wasmtime) — sandbox'langan, tez yuklanadigan pluginlar uchun qo'shimcha variant
- [x] Plugin yon panellari (`[[panels]]`, `set_panel`)
- [ ] Plugin UI kengaytmalari: status bar elementlari

**v1.0**
- [ ] Plugin marketplace / registry
- [x] Paketlar: .dmg (macOS), .zip (Windows), .deb va .tar.gz (Linux)
- [ ] Kod imzosi (Apple notarization, Windows Authenticode), .msi, AppImage, avtomatik yangilanish
- [x] Sozlamalar oynasi
