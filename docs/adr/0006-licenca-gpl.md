# ADR-0006 — Licenciamento sob GPL-3.0-or-later

**Status:** Aceito
**Data:** 2026-11-15
**Relacionados:** [ADR-0005](0005-empacotamento-binario-e-docker.md), [ADR-0010 do Porecatu](https://github.com/porecatu/terminal/blob/main/docs/adr/0010-licenciamento.md)

## Contexto

O Porecatu é GPL-3.0-or-later. O relay é um projeto separado, com repositório próprio, que se comunica com o Porecatu só pela rede, por um protocolo especificado em outro repositório. A licença dele não é imposta pela do Porecatu: um servidor que fala um protocolo não é obra derivada do cliente.

A escolha pesa duas coisas: coerência com o resto do ecossistema e o fato de o relay ser um **serviço de rede** — a GPL não obriga quem o modifica e o oferece como serviço a publicar as mudanças; a AGPL obrigaria.

O dono do produto decidiu, antes da escrita: **tudo GPL-3.0-or-later**.

## Decisão

**O Porecatu Relay é licenciado sob GPL-3.0-or-later.**

- `LICENSE` é a cópia verbatim do texto da FSF, **byte a byte igual** à do Porecatu (sha256 `3972dc9744f6499f0f9b2dbf76696f2ae7ad8af9b23dde66d6af86c9dfb36986`), e nunca é editado.
- Todo arquivo de código-fonte e script começa com `SPDX-License-Identifier: GPL-3.0-or-later`.
- Contribuições entram sob a mesma licença, com `Signed-off-by` (DCO) — ver [CONTRIBUTING.md](../../CONTRIBUTING.md).
- Dependência nova tem licença conferida contra GPLv3 antes de entrar (`cargo deny` no CI): MIT, Apache-2.0, BSD, ISC, Zlib, MPL-2.0 são compatíveis; o `ring` (ISC e afins) também.
- Os binários e a imagem de contêiner levam o `LICENSE` junto ([ADR-0005](0005-empacotamento-binario-e-docker.md)).

## Alternativas consideradas

- **AGPL-3.0-or-later.** Fecharia o "furo de serviço": quem modificasse o relay e o oferecesse publicamente teria de publicar o código. Era a recomendação inicial. Rejeitada pelo dono em favor de uma licença só no ecossistema; e o relay não tem relay público oficial nem é o lugar onde há valor a proteger (é um roteador de bytes cegos), o que reduz o peso do furo.
- **MIT ou Apache-2.0.** Facilitaria adoção corporativa. Rejeitada pela mesma decisão de uniformidade.

## Consequências

### Positivas

- Uma licença só nos quatro repositórios; nenhuma dúvida sobre copiar código entre eles.
- Distribuições do binário modificado vêm com fonte.

### Negativas

- Um relay modificado operado como serviço não obriga a publicar as mudanças.

### Riscos e mitigação

| Risco | Probabilidade | Impacto | Mitigação |
|---|---|---|---|
| Fork fechado oferecido como serviço | baixa | baixo | Aceito; o protocolo é aberto e o relay é cego, então um fork não ganha acesso a conteúdo |
| Dependência com licença incompatível entra sem ninguém ver | baixa | médio | `cargo deny check licenses` no CI |
