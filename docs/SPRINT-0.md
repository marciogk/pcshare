# Sprint 0 — Spike de latência

**Duração alvo:** 2 a 4 semanas
**Objetivo:** provar ou refutar o risco técnico número 1 antes de investir meses.

---

## O que este spike é

Uma pipeline mínima de ponta a ponta, na rede local:

```
DXGI Desktop Duplication  (com dirty rects)
        ▼
Encoder H.264 por hardware  (Media Foundation / Quick Sync)
        ▼
Transport::send  ──── TCP na LAN ────►  Transport::recv
                                              ▼
                                        Decoder H.264
                                              ▼
                                        Janela do viewer
                                              ▼
                                   eventos de mouse/teclado
                                              │
        ◄─────────────────────────────────────┘
                    SendInput no host
```

## O que este spike NÃO é

Deliberadamente fora de escopo — incluir qualquer um destes atrasa a resposta
à única pergunta que importa agora:

- ❌ Criptografia
- ❌ Contas, login, IDs
- ❌ NAT traversal / WebRTC / servidor de sinalização
- ❌ Windows Service, UAC, tela de bloqueio
- ❌ Instalador
- ❌ Interface bonita
- ❌ Multi-monitor
- ❌ Transferência de arquivos, clipboard, áudio

---

## Critério de saída (mensurável)

| Métrica | Alvo | Como medir |
|---|---|---|
| Taxa de quadros | ≥ 30 fps a 1920×1080 | contador no viewer |
| Latência glass-to-glass | < 80 ms na LAN | ver protocolo abaixo |
| CPU no host | < 15% de um core moderno | Gerenciador de Tarefas |
| Banda, tela parada | < 100 kbps | contador de bytes no transporte |
| Banda, vídeo em tela cheia | < 8 Mbps | contador de bytes no transporte |

### Como medir latência glass-to-glass corretamente

Não confie em timestamps internos — eles não medem o que o usuário sente.

1. Abra um cronômetro com milissegundos na tela do **host**.
2. Coloque a janela do **viewer** ao lado, exibindo esse mesmo cronômetro.
3. Fotografe as duas telas juntas com um celular.
4. A diferença entre os dois números **é** a latência glass-to-glass.
5. Repita 10 vezes, use a mediana.

Isso captura tudo: captura, encode, rede, decode, e o tempo de apresentação do
compositor — inclusive os custos que instrumentação interna não enxerga.

---

## Decisão ao final do sprint

- **Bateu as metas** → o resto do projeto é trabalho conhecido. Segue para a Fase 1.
- **Chegou perto** (ex.: 25 fps, 110 ms) → identificar o gargalo dominante e
  decidir se é ajuste ou problema estrutural.
- **Ficou longe** → parar e reavaliar todo o plano de negócio **antes** de
  gastar meses. Este é o propósito do spike.

---

## Ordem de ataque sugerida

Cada etapa é verificável isoladamente. Não avance com uma etapa "quase funcionando".

1. **Captura pura.** DXGI Desktop Duplication num loop, descartando os frames.
   Medir: quantos frames/s a API entrega e quantas dirty rects por frame.
   *Já revela se a HD 4600 aguenta o caminho.*
2. **Encode.** Ligar o Media Foundation H.264. Gravar 10 s em arquivo e abrir
   no player. Medir: tempo de encode por frame.
3. **Transporte local.** Host e viewer no mesmo PC, via `127.0.0.1`.
   Elimina a rede como variável.
4. **Decode e exibição.** Primeira imagem na tela. Medir latência aqui.
5. **Duas máquinas na LAN.** Medir de novo — a diferença é o custo da rede.
6. **Input de volta.** Mouse e teclado do viewer para o host.

---

## Notas de implementação

- Manter host e viewer como **um único processo cada**. A separação
  serviço/agent (decisão D5) vem na Fase 3; antecipá-la aqui só adiciona atrito.
- Todo I/O de rede passa pelo trait `Transport`. Nenhum `TcpStream` deve
  aparecer no código de captura ou de encode. Ver decisão D4.
- Instrumentar desde o primeiro dia: fps, bytes enviados, tempo de encode.
  Otimizar sem medir é perda de tempo garantida.
