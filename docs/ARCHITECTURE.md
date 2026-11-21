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
- [ ] LSP: rename, find references, format on save, signature help, code actions
- [ ] Git: yonma-yon diff ko'rinishi, branch almashtirish, hunk'ni qaytarish (revert)

**v0.3**
- [ ] Tree-sitter asosida ranglash va kod katlamlari (folding)
- [ ] MCP serverlarini sozlash UI
- [ ] WASM pluginlar (wasmtime) — sandbox'langan, tez yuklanadigan pluginlar uchun qo'shimcha variant
- [ ] Plugin UI kengaytmalari: status bar elementlari, yon panel ko'rinishlari

**v1.0**
- [ ] Plugin marketplace / registry
- [ ] Imzolangan installerlar (.dmg, .msi, .deb/.AppImage)
- [ ] Sozlamalar UI
