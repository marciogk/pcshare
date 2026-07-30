use std::io;

/// Qual garantia de entrega a mensagem exige.
///
/// A distinção existe por causa da Fase 2. Sobre TCP os dois canais acabam
/// confiáveis e ordenados — TCP não sabe fazer diferente. Mas registrar a
/// *intenção* agora é o que permite ao transporte WebRTC honrá-la depois, sem
/// reabrir cada ponto de envio para descobrir o que podia ser descartado.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Confiável e ordenado. Handshake, eventos de entrada, pedido de
    /// keyframe. Perder um clique é inaceitável.
    Control,
    /// Melhor esforço. Vídeo. Um frame que chega atrasado vale menos que um
    /// frame descartado: retransmitir dado velho só piora a latência do que
    /// vem depois.
    Media,
}

/// Um canal bidirecional de mensagens entre host e viewer.
///
/// Deliberadamente orientado a mensagens, não a bytes: o transporte WebRTC
/// entrega datagramas, e uma API de stream obrigaria a reimplementar
/// enquadramento em cima dele.
pub trait Transport: Send {
    /// Enfileira uma mensagem.
    ///
    /// **Não garante entrega.** Quando o buffer do socket enche, o restante
    /// fica pendente e só escoa em chamadas posteriores a [`Self::flush`] ou
    /// [`Self::poll`]. Um laço que só envia, sem nunca bombear, trava com os
    /// dados presos na memória — e em vídeo o buffer enche em fração de
    /// segundo.
    fn send(&mut self, channel: Channel, payload: &[u8]) -> io::Result<()>;

    /// Devolve a próxima mensagem recebida, ou `None` se ainda não há nenhuma
    /// completa. Nunca bloqueia.
    fn poll(&mut self) -> io::Result<Option<(Channel, Vec<u8>)>>;

    /// Empurra o que estiver pendente. Chamar ao fim de um lote de envios.
    fn flush(&mut self) -> io::Result<()>;

    /// Bytes entregues ao sistema operacional desde a criação.
    fn bytes_sent(&self) -> u64;
    fn bytes_received(&self) -> u64;
}
