//! Conversão entre BGRA (o que a captura entrega) e NV12 (o que o encoder
//! aceita), e a volta para exibição.
//!
//! Custo de CPU puro que a decisão D3 manda eliminar no produto — o caminho
//! definitivo mantém tudo na GPU. Existe aqui porque o Sprint 0 precisa medir
//! quanto esse desvio custa antes de decidir se vale a complexidade de
//! eliminá-lo já na Fase 1.
//!
//! Matriz BT.601 de faixa limitada (luma 16..235), em ponto fixo. A escolha
//! importa menos do que a **consistência**: as duas direções usam a mesma
//! matriz, então o ida-e-volta preserva a cor. Trocar só um dos lados produz
//! imagem lavada ou saturada demais.

/// BGRA de 8 bits por canal, linhas contíguas de `width * 4` bytes.
/// `out` recebe NV12: plano Y de `width*height`, seguido do plano UV
/// entrelaçado com metade das linhas.
///
/// Dimensões ímpares são truncadas para baixo: NV12 subamostra croma em
/// blocos 2x2 e não representa a linha ou coluna sobrando.
pub fn bgra_to_nv12(bgra: &[u8], width: usize, height: usize, out: &mut Vec<u8>) {
    debug_assert!(bgra.len() >= width * height * 4, "buffer BGRA curto demais");
    let (w2, h2) = (width / 2, height / 2);
    out.resize(width * height + w2 * h2 * 2, 0);
    let (y_plane, uv_plane) = out.split_at_mut(width * height);

    for y in 0..height {
        let src = &bgra[y * width * 4..(y * width + width) * 4];
        let dst = &mut y_plane[y * width..y * width + width];
        for x in 0..width {
            let p = &src[x * 4..x * 4 + 4];
            dst[x] = luma(p[2], p[1], p[0]);
        }
    }

    // Croma a partir da média de cada bloco 2x2. Média em BGR antes de
    // converter é mais barata que converter os quatro e depois somar, e a
    // diferença visual é imperceptível.
    for by in 0..h2 {
        let row0 = &bgra[(by * 2) * width * 4..];
        let row1 = &bgra[(by * 2 + 1) * width * 4..];
        let dst = &mut uv_plane[by * width..by * width + w2 * 2];
        for bx in 0..w2 {
            let i = bx * 2 * 4;
            let mut b = 0u32;
            let mut g = 0u32;
            let mut r = 0u32;
            for px in [&row0[i..i + 8], &row1[i..i + 8]] {
                b += px[0] as u32 + px[4] as u32;
                g += px[1] as u32 + px[5] as u32;
                r += px[2] as u32 + px[6] as u32;
            }
            let (b, g, r) = ((b / 4) as u8, (g / 4) as u8, (r / 4) as u8);
            dst[bx * 2] = chroma_u(r, g, b);
            dst[bx * 2 + 1] = chroma_v(r, g, b);
        }
    }
}

/// Inverso, para exibir o que o decodificador devolveu.
///
/// `src_stride` é o passo de linha do plano Y no buffer de origem: o
/// decodificador alinha as linhas e ele raramente é igual a `width`.
pub fn nv12_to_bgra(
    nv12: &[u8],
    width: usize,
    height: usize,
    src_stride: usize,
    out: &mut Vec<u8>,
) {
    debug_assert!(src_stride >= width, "stride menor que a largura");
    out.resize(width * height * 4, 0xFF);
    let uv_offset = src_stride * height;

    for y in 0..height {
        let y_row = &nv12[y * src_stride..y * src_stride + width];
        let uv_row = &nv12[uv_offset + (y / 2) * src_stride..];
        let dst = &mut out[y * width * 4..(y * width + width) * 4];

        for x in 0..width {
            let c = y_row[x] as i32 - 16;
            let u = uv_row[(x / 2) * 2] as i32 - 128;
            let v = uv_row[(x / 2) * 2 + 1] as i32 - 128;

            let c = 298 * c;
            let px = &mut dst[x * 4..x * 4 + 4];
            px[0] = clamp8((c + 516 * u + 128) >> 8); // B
            px[1] = clamp8((c - 100 * u - 208 * v + 128) >> 8); // G
            px[2] = clamp8((c + 409 * v + 128) >> 8); // R
            px[3] = 0xFF;
        }
    }
}

fn luma(r: u8, g: u8, b: u8) -> u8 {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8
}

fn chroma_u(r: u8, g: u8, b: u8) -> u8 {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    clamp8(((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128)
}

fn chroma_v(r: u8, g: u8, b: u8) -> u8 {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    clamp8(((112 * r - 94 * g - 18 * b + 128) >> 8) + 128)
}

fn clamp8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bgra_solid(width: usize, height: usize, b: u8, g: u8, r: u8) -> Vec<u8> {
        [b, g, r, 0xFF].repeat(width * height)
    }

    /// O ida-e-volta não é exato — subamostragem de croma e ponto fixo perdem
    /// informação. O que precisa valer é que a cor não *deriva*.
    fn roundtrip_cor(b: u8, g: u8, r: u8, tol: i32) {
        let (w, h) = (16, 16);
        let src = bgra_solid(w, h, b, g, r);
        let mut nv12 = Vec::new();
        bgra_to_nv12(&src, w, h, &mut nv12);

        let mut back = Vec::new();
        nv12_to_bgra(&nv12, w, h, w, &mut back);

        for (i, name) in [(0, "B"), (1, "G"), (2, "R")] {
            let got = back[i] as i32;
            let want = [b, g, r][i] as i32;
            assert!(
                (got - want).abs() <= tol,
                "canal {name}: esperava ~{want}, veio {got}"
            );
        }
        assert_eq!(back[3], 0xFF, "alfa deve ser opaco");
    }

    #[test]
    fn cores_sobrevivem_ao_ida_e_volta() {
        roundtrip_cor(0, 0, 0, 3);
        roundtrip_cor(255, 255, 255, 3);
        roundtrip_cor(255, 0, 0, 6);
        roundtrip_cor(0, 255, 0, 6);
        roundtrip_cor(0, 0, 255, 6);
        roundtrip_cor(128, 64, 192, 6);
    }

    #[test]
    fn tamanho_do_nv12_e_o_esperado() {
        let (w, h) = (64, 48);
        let mut nv12 = Vec::new();
        bgra_to_nv12(&bgra_solid(w, h, 10, 20, 30), w, h, &mut nv12);
        assert_eq!(nv12.len(), w * h * 3 / 2);
    }

    /// O decodificador entrega linhas alinhadas; ignorar isso embaralha a
    /// imagem em qualquer largura não alinhada.
    #[test]
    fn stride_maior_que_a_largura_e_respeitado() {
        let (w, h, stride) = (4, 4, 8);
        let mut nv12 = vec![0u8; stride * h + stride * (h / 2)];
        // Luma cresce da esquerda para a direita; o preenchimento além de `w`
        // fica em 0 e não pode aparecer na saída.
        for y in 0..h {
            for x in 0..w {
                nv12[y * stride + x] = 16 + (x as u8) * 50;
            }
        }
        for i in 0..stride * (h / 2) {
            nv12[stride * h + i] = 128;
        }

        let mut out = Vec::new();
        nv12_to_bgra(&nv12, w, h, stride, &mut out);

        for y in 0..h {
            let mut anterior = -1i32;
            for x in 0..w {
                let v = out[(y * w + x) * 4 + 1] as i32;
                assert!(v > anterior, "linha {y} não é monotônica em x={x}");
                anterior = v;
            }
        }
    }
}
