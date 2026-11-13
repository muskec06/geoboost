# CLAUDE.md

## Proje özeti

GeoBoost, yüksek gecikmeli GEO uydu bağlantılarını (uçak içi Wi-Fi, VSAT, denizcilik interneti) Windows'ta hızlandıran küçük bir Rust uygulamasıdır. Trafiği satmux v3 protokolüyle kendi yer sunucusuna taşıyarak gidiş-dönüş (round trip) sayısını azaltır. Tek bir `.exe` olarak çalışır; kurulum veya yönetici yetkisi gerekmez.

- `src/` — Rust istemcisi (`mux.rs` protokol, `proxy.rs` yerel SOCKS5/HTTP proxy, `app.rs` bağlantı yönetimi, `panel.rs` + `panel.html` yerel kontrol paneli, `win.rs` Windows entegrasyonu, `nowin.rs` Windows dışı başsız mod)
- `server/` — yer sunucusu tarafı (`satmux.py`, `install-server.sh`)
- `tools/` — ölçüm ve grafik/diyagram üretme betikleri
- `docs/images/` — README görselleri

## Derleme ve test

```powershell
cargo build --release      # çıktı: target\release\geoboost.exe
cargo test --release       # protokol testleri
```

Linux'ta aynı ikili başsız (tepsi simgesi olmadan) çalışır ve protokol testi için kullanılır:

```bash
GEOBOOST_HOME=/tmp/gb cargo run --release -- --connect
```

GitHub Actions (`.github/workflows/build.yml`) her push ve PR'da Linux'ta testleri, Windows'ta derlemeyi çalıştırır; `v*` etiketlerinde `GeoBoost-windows-x64.zip` sürüme eklenir.

## Çalışma kuralı

**Her zaman yeni bir dalda çalış, main'e doğrudan commit atma, işi pull request olarak sun.**

`main` dalı GitHub'da bir ruleset ile korunuyor: doğrudan push, zorla push ve dal silme engelli; değişiklikler yalnızca pull request ile girebilir.
