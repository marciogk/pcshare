# Próximos passos — guia prático

Passo a passo das ações pendentes, em ordem de prioridade.
Nenhuma exige escrever código.

---

## 1. Desligar o Smart App Control  🔴 bloqueia tudo

**Por que:** cada vez que o projeto é recompilado, o Windows avalia o
executável novo e às vezes o bloqueia com *"Uma política de Controle de
Aplicativo bloqueou este arquivo"*. Isso já derrubou a suíte de testes duas
vezes. Enquanto não for resolvido, cada compilação é um sorteio.

**Antes de fazer, entenda:** desligar é **irreversível**. O Windows não
permite religar o Smart App Control depois; só uma reinstalação limpa do
sistema o traz de volta. Numa máquina de desenvolvimento isso costuma valer a
pena, mas a decisão é sua.

**Como:**

1. Tecla Windows → digite `Segurança do Windows` → Enter
2. No menu da esquerda, clique em **Controle de aplicativo e navegador**
3. Clique em **Configurações de Controle Inteligente de Aplicativos**
   (ou *Smart App Control settings*)
4. Marque **Desativado**
5. Confirme o aviso
6. **Reinicie o computador**

**Como confirmar que funcionou** — abra o PowerShell e rode:

```powershell
(Get-ItemProperty "HKLM:\SYSTEM\CurrentControlSet\Control\CI\Policy").VerifiedAndReputablePolicyState
```

Deve responder `0`. Se responder `1`, ainda está ligado.

Depois disso, rode a suíte três vezes seguidas para confirmar que ficou
estável:

```powershell
cd "c:\Projeto Pcshare"
cargo test --workspace
cargo test --workspace
cargo test --workspace
```

---

## 2. ~~Conferir os arquivos .h264 no VLC~~ ✅ resolvido

Não é mais necessário. A verificação foi feita gravando um quadro decodificado
como imagem e conferindo diretamente: a cadeia inteira (captura → BGRA → NV12 →
H.264 → rede → H.264 → NV12 → BGRA → tela) reproduz a tela com texto legível,
cores corretas e sem distorção.

Se quiser ver com os próprios olhos, o quadro está em `spike-output/quadro.bmp`
— basta dar duplo clique.

---

## 3. Medir a latência glass-to-glass  🟡 o número que falta

**Por que:** os 16,5 ms medidos são de relógio interno. Eles não incluem o
tempo que o Windows leva para pôr o pixel no monitor nem o tempo que o monitor
leva para acendê-lo. O critério do Sprint 0 é sobre a latência que o **usuário
sente**, e ela só se mede com câmera.

**O que você precisa:** um celular com câmera.

**Como:**

1. Abra um cronômetro com milissegundos na tela. Sugestão sem instalar nada:
   busque `cronômetro online milissegundos` no navegador e deixe rodando.
2. Abra o PowerShell e rode:

   ```powershell
   cd "c:\Projeto Pcshare"
   .\target\release\pcshare-spike.exe --view-only
   ```

3. Vai abrir a janela do viewer. Arraste-a para o lado do cronômetro, de modo
   que você veja **os dois ao mesmo tempo**: o cronômetro real e o cronômetro
   reproduzido dentro da janela.
4. Fotografe a tela inteira com o celular.
5. Na foto, subtraia: `número real − número dentro da janela`. **Essa
   diferença é a latência glass-to-glass.**
6. Repita umas 10 vezes e use o valor do meio.

**Meta:** abaixo de 80 ms. Anote o resultado — ele fecha o Sprint 0.

Para fechar a janela, clique no X dela.

---

## 4. Etapa 5 — testar entre duas máquinas  🟢 quando tiver um segundo PC

**Por que:** tudo até aqui rodou em `127.0.0.1`, que não tem rede de verdade.
Falta provar que funciona entre dois computadores.

**O que você precisa:** um segundo PC com Windows na mesma rede (mesmo
roteador, mesmo Wi-Fi).

**Na máquina que será controlada (host):**

1. Descubra o IP dela:

   ```powershell
   ipconfig | Select-String "IPv4"
   ```

   Anote algo como `192.168.0.15`.

2. Copie a pasta `c:\Projeto Pcshare\target\release\` para essa máquina, ou
   compile o projeto lá.

3. Rode:

   ```powershell
   .\pcshare-spike.exe host 0.0.0.0:7000
   ```

4. O Windows vai perguntar sobre o firewall na primeira vez. **Permita em
   redes privadas.** Se não aparecer nada e a conexão falhar, libere a porta
   manualmente num PowerShell **como administrador**:

   ```powershell
   New-NetFirewallRule -DisplayName "PCShare spike" -Direction Inbound `
     -Protocol TCP -LocalPort 7000 -Action Allow -Profile Private
   ```

**Na máquina que vai controlar (viewer):**

```powershell
.\pcshare-spike.exe viewer 192.168.0.15:7000
```

Troque pelo IP anotado. Deve abrir a janela mostrando a tela da outra máquina.

> ⚠️ **Sem `--view-only`, o mouse e o teclado do viewer controlam a outra
> máquina de verdade.** É o comportamento pretendido — mas só faça isso numa
> máquina sua, e confirme que sabe voltar a ela fisicamente.

**O que observar e anotar:**

- A latência reportada a cada 2 s ainda fica estável, ou volta a crescer?
- Aparece `desc` (descartes por contrapressão) maior que zero?
- Quantos Mbps na rede real?

Anote a saída do terminal — é o resultado da etapa 5.

---

## 5. Ainda não medido: CPU no host

Rápido de fazer junto com qualquer teste acima: com o `pcshare-spike` rodando,
abra o Gerenciador de Tarefas (Ctrl+Shift+Esc), aba **Detalhes**, procure
`pcshare-spike.exe` e anote a coluna de CPU.

**Meta:** abaixo de 15% de um núcleo.

---

## Decisão de negócio que não é técnica

O Smart App Control bloqueando o seu próprio desenvolvimento é uma amostra do
que ele fará com o instalador na máquina de cada cliente novo — e ele vem
ligado por padrão em instalações limpas de Windows 11.

Antes de definir preço e data de lançamento, vale responder:

- Um certificado **OV** basta para passar pelo Smart App Control, ou o **EV** é
  obrigatório?
- Quanto tempo leva para acumular reputação junto à Microsoft partindo do zero?
- Quanto custa cada opção, por ano?

Isso afeta o plano de negócio, não o código. Ver risco R1 em
[ARQUITETURA.md](ARQUITETURA.md).
