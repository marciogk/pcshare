# PCShare — Arquitetura

Documento vivo. Registra as decisões estruturais e o *porquê* de cada uma.
Alterar uma decisão aqui é barato; alterar depois de 5.000 linhas de código, não.

---

## Princípio norteador

Três riscos técnicos definem se este produto existe. Todos devem ser atacados
**antes** de qualquer investimento em contas, billing ou painel web:

1. **Latência e qualidade de imagem** — se não for fluido, não é vendável.
2. **UAC / Secure Desktop** — se não funcionar na tela de UAC e de bloqueio,
   não serve para suporte remoto real.
3. **Reputação junto a antivírus** — se o instalador for bloqueado, não chega
   ao cliente.

Tudo o mais é trabalho conhecido.

---

## Topologia

```
┌──────────────────┐                          ┌──────────────────┐
│  HOST            │                          │  VIEWER          │
│  (controlado)    │                          │  (controlador)   │
│                  │                          │                  │
│  captura DXGI    │◄──── input events ──────►│  janela + decode │
│  encode H.264    │───── video stream ──────►│                  │
└────────┬─────────┘                          └────────┬─────────┘
         │                                             │
         │         ┌───────────────────────┐           │
         └────────►│  SINALIZAÇÃO          │◄──────────┘
                   │  troca de SDP/ICE     │
                   │  autenticação, IDs    │
                   │  NUNCA vê o vídeo     │
                   └───────────┬───────────┘
                               │
                   ┌───────────▼───────────┐
                   │  TURN (coturn)        │
                   │  fallback quando P2P   │
                   │  não fecha (~10-20%)  │
                   └───────────────────────┘
```

---

## Decisões

### D1 — Rust no cliente

Binário único, sem runtime, sem dependência de VC++ redistributable.
Importa por três motivos, em ordem de peso:

1. Menor superfície para falso-positivo de antivírus que um Electron de 150 MB.
2. Controle sobre alocação e cópias no caminho do vídeo (onde a latência mora).
3. Bindings oficiais da Microsoft para as APIs do Windows (crate `windows`).

**Descartado:** Electron. Rápido para MVP, mas não resolve secure desktop e tem
exatamente o perfil que antivírus sinaliza em software de controle remoto.

### D2 — Captura via DXGI Desktop Duplication

Entrega a textura já na GPU **e as dirty rects** — a lista de retângulos que
mudaram desde o último frame.

As dirty rects são o ponto central de desempenho, não um detalhe. Numa tela de
escritório parada, quase nenhum pixel muda entre frames. Codificar a tela
inteira a 30 fps é desperdiçar 95% da banda e do CPU.

**Descartado:** `mss`, GDI `BitBlt`, screenshots. Não expõem dirty rects,
copiam da GPU para a CPU a cada frame, e ensinam o padrão errado.

### D3 — Encode H.264 por hardware (Media Foundation)

Quick Sync / NVENC / AMF conforme a GPU disponível, com fallback para encoder
por software (openh264) quando não houver hardware.

H.264 e não AV1/HEVC: é o único com encoder por hardware universalmente
presente. AV1 é melhor, mas exclui toda máquina anterior a ~2021 — inaceitável
num produto cujo cliente instala na máquina alheia.

> **Máquina de desenvolvimento atual: Intel HD Graphics 4600 (Haswell, 2013),
> monitor 1360×768 nativo.** Tem Quick Sync H.264, não tem HEVC nem AV1.
> Tratar como **piso de compatibilidade**: se ficar fluido nela, roda em
> qualquer lugar. Não usar para medir o teto de qualidade.
>
> A resolução do monitor não permite validar 1080p ponta a ponta. Encode é
> testado com frame sintético — ver a seção de restrição em `SPRINT-0.md`.

### D4 — Transporte atrás de uma abstração, desde a primeira linha

O Sprint 0 usa TCP na LAN. A Fase 2 troca por WebRTC/ICE.

Essa troca **não pode** exigir reescrever a pipeline de vídeo. O host e o
viewer conversam com um trait `Transport`; quem implementa é detalhe.

Sem essa costura, a migração para WebRTC vira uma reescrita.

### D5 — Arquitetura de dois processos no host (a partir da Fase 3)

```
pcshare-service.exe    → Windows Service, sessão 0, SYSTEM
        │                 sobrevive a logoff, vê a tela de bloqueio
        │ WTSQueryUserToken + CreateProcessAsUser
        ▼
pcshare-agent.exe      → um por sessão de usuário
                         captura, encode, injeta input
                         se anexa ao desktop Winlogon durante UAC
```

Este é o item de maior risco de cronograma do projeto inteiro. Não é opcional
e não dá para "adicionar depois" — a separação de processos precisa estar
prevista no design desde já, mesmo que o Sprint 0 rode como processo único.

### D6 — Sinalização não trafega vídeo

Servidor de sinalização faz autenticação, resolve ID → peer, e intermedeia a
troca de SDP/ICE. Uma vez estabelecida a sessão, sai do caminho.

**Consequência de negócio:** quando o P2P não fecha, o tráfego cai no TURN e
*toda* a banda de vídeo passa por você. É o principal custo variável de
infraestrutura e precisa estar na precificação desde o começo.

### D7 — Criptografia ponta a ponta, com verificação de identidade

DTLS-SRTP do WebRTC já dá E2E entre os peers, inclusive através do TURN (o
relay só encaminha pacotes cifrados).

Mas isso sozinho **não** protege contra MITM feito pelo próprio servidor de
sinalização. É necessário verificar a chave pública do peer fora da banda —
modelo ID + senha, com fingerprint fixado no primeiro pareamento.

### D8 — Esperar por evento, nunca por relógio

Nenhum laço do caminho de vídeo ou de input pode usar `thread::sleep` para
aguardar trabalho. Só espera bloqueante em evento: `AcquireNextFrame` da
captura, `GetEvent` do MFT, leitura bloqueante ou IOCP no socket.

**Medido na etapa 3:** um laço com `sleep(200 µs)` produziu RTT de 1,27 ms em
127.0.0.1; o mesmo laço com `yield_now` produziu 0,061 ms. Fator de 21. O
Windows não honra sleeps de sub-milissegundo — o pedido de 200 µs virou mais de
1 ms de espera real.

Num orçamento de 80 ms atravessando captura, encode, rede, decode e
apresentação, cada estágio que espera por relógio em vez de evento doa mais de
1 ms para nada.

> **Não use spin como solução.** `yield_now` num laço queima um núcleo, e
> "cliente leve" é requisito de produto (decisão D1). O spin serviu para
> *medir* o custo do sleep, não para eliminá-lo. A resposta é espera
> bloqueante.

---

## Riscos não-técnicos com impacto em arquitetura

### R1 — Assinatura de código  ⚠️ JÁ OBSERVADO

Sem certificado, o SmartScreen bloqueia a instalação e antivírus dão
falso-positivo. Custo recorrente (OV ~R$1.500–3.000/ano; EV mais caro, mas
dispensa a reputação acumulada).

Consequência de arquitetura: número de binários assinados deve ser mínimo, e a
cadeia de atualização automática precisa validar assinatura.

> **Aconteceu na etapa 4, na máquina de desenvolvimento.** O
> `pcshare-spike.exe` rodou uma vez e, na recompilação seguinte, foi bloqueado:
> *"Uma política de Controle de Aplicativo bloqueou este arquivo"*. A máquina
> tem **Smart App Control em imposição**
> (`HKLM:\SYSTEM\CurrentControlSet\Control\CI\Policy` →
> `VerifiedAndReputablePolicyState = 1`).
>
> Isto eleva R1 de incômodo a **restrição de entrada no mercado**. O Smart App
> Control vem ligado por padrão em instalações limpas de Windows 11 e bloqueia
> executáveis sem assinatura confiável ou reputação acumulada — exatamente o
> perfil de um produto novo de acesso remoto. O cliente que mais precisa da
> instalação simples é justamente o de máquina nova.
>
> A verificar antes de definir preço e prazo de lançamento: se um certificado
> OV basta ou se o EV é obrigatório para passar pelo Smart App Control, e
> quanto tempo leva para acumular reputação a partir do zero.

### R2 — Abuso por golpistas (mercado brasileiro)

Acesso remoto é o vetor principal do golpe do falso suporte no Brasil.

Requisitos derivados, obrigatórios já no MVP:

- Indicador visual **persistente e não-ocultável** de sessão ativa na máquina controlada.
- Trilha de auditoria de sessões (quem, quando, de onde, duração).
- Rate limit agressivo em contas e IDs recém-criados.
- Mecanismo de bloqueio/banimento de conta.

### R3 — Bloqueio por aplicativos bancários

Apps de banco detectam e bloqueiam sessões de acesso remoto ativo. Testar cedo
se o nicho escolhido tiver qualquer proximidade com financeiro.

### R4 — LGPD

Sessão remota acessa dados pessoais de terceiros. Consentimento explícito
registrado e trilha de auditoria não são opcionais.

---

## Fases

| Fase | Escopo | Critério de saída |
|---|---|---|
| **0** | Spike LAN: captura → encode → TCP → decode → exibe → input | 30 fps @ 1080p, latência glass-to-glass < 80 ms na LAN |
| **1** | Criptografia, ID + senha, pareamento com fingerprint | Sessão segura entre duas máquinas na LAN |
| **2** | WebRTC/ICE, servidor de sinalização, TURN | Conexão entre redes distintas, atrás de NAT |
| **3** | Windows Service + agent, UAC, tela de bloqueio, instalador | Suporte remoto real numa máquina que você não pré-configurou |
| **4** | Contas, planos, painel, atualização automática, auditoria | Produto vendável |
