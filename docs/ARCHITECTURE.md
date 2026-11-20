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

**v0.1 (hozirgi)** — muharrir, terminal, AI agent panel, pluginlar, qidiruv, temalar, CI (3 OS).

**v0.2**
- [ ] Ko'p kursor (multi-cursor) va ustun bo'yicha belgilash
- [ ] LSP mijozi: avtomatik to'ldirish, xatolar, "go to definition", hover (rust-analyzer, pyright, tsserver…)
- [ ] Git integratsiyasi: o'zgarishlar gutter'da, diff ko'rinishi, commit paneli
- [ ] Faylni tashqaridan o'zgarganda konflikt dialogi (hozir: toza bufer avtomatik qayta yuklanadi)
- [ ] Split editor (yonma-yon)

**v0.3**
- [ ] Tree-sitter asosida ranglash va kod katlamlari (folding)
- [ ] Inline AI: tanlangan kod ustida agent taklif qilgan diff'ni qabul qilish/rad etish
- [ ] MCP serverlarini sozlash UI
- [ ] WASM pluginlar (wasmtime) — sandbox'langan, tez yuklanadigan pluginlar uchun qo'shimcha variant
- [ ] Plugin UI kengaytmalari: status bar elementlari, yon panel ko'rinishlari

**v1.0**
- [ ] Plugin marketplace / registry
- [ ] Imzolangan installerlar (.dmg, .msi, .deb/.AppImage)
- [ ] Sozlamalar UI
