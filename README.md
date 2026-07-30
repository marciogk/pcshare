# PCShare

Software de acesso e controle remoto para Windows.

**Status:** Fase 0 — spike de latência. Nada funcional ainda.

## Documentação

- [Arquitetura e decisões](docs/ARQUITETURA.md) — leia antes de escrever código
- [Sprint 0](docs/SPRINT-0.md) — o spike atual e seu critério de saída

## Estrutura pretendida

```
pcshare-core      tipos do protocolo, trait Transport
pcshare-capture   captura de tela (DXGI Desktop Duplication)
pcshare-codec     encode/decode H.264 (Media Foundation)
pcshare-host      lado controlado
pcshare-viewer    lado controlador
```

## Ambiente de desenvolvimento

| Ferramenta | Status |
|---|---|
| Rust | ✅ 1.97.1 |
| VS Build Tools + Windows SDK | ✅ 17.14 (o linker `link.exe` não vem com o rustup) |
| Git | ✅ 2.55.0 |
| Node.js | ✅ v26.5.0 (para o servidor de sinalização, Fase 2) |
| Python | ✅ 3.14.3 (ferramentas internas apenas) |

Reinstalação num ambiente limpo:

```powershell
winget install Git.Git
winget install Rustlang.Rustup
winget install Microsoft.VisualStudio.2022.BuildTools --override `
  "--wait --quiet --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
```

## Benchmarks do Sprint 0

```powershell
cargo test                                              # protocolo e transporte
cargo run --release -p pcshare-capture --bin bench       # etapa 1: captura
cargo run --release -p pcshare-codec  --bin encbench     # etapa 2: encoder
cargo run --release -p pcshare-core   --bin linkbench    # etapa 3: transporte
```
