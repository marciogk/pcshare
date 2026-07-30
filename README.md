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
| Rust (rustup) | ⬜ a instalar |
| Git | ⬜ a instalar |
| Node.js | ✅ v26.5.0 (para o servidor de sinalização, Fase 2) |
| Python | ✅ 3.14.3 (ferramentas internas apenas) |

Instalação:

```powershell
winget install Git.Git
winget install Rustlang.Rustup
```

Após instalar, abra um terminal novo (o PATH precisa recarregar) e confirme:

```powershell
git --version
cargo --version
```
