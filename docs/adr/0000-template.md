# ADR-0000 — Template e índice

## Índice de decisões

| # | Título | Status |
|---|--------|--------|
| [0001](0001-rust-tokio-e-axum.md) | Rust, `tokio` e `axum` | Aceito |
| [0002](0002-relay-cego-e-sem-estado-persistente.md) | Relay cego e sem estado persistente | Aceito |
| [0003](0003-registro-do-host-por-chave-e-token-do-operador.md) | Registro do host por chave e token do operador | Aceito |
| [0004](0004-tls-embutido-ou-atras-de-proxy.md) | TLS embutido ou atrás de proxy reverso | Aceito |
| [0005](0005-empacotamento-binario-e-docker.md) | Empacotamento: binário estático e imagem de contêiner | Aceito |
| [0006](0006-licenca-gpl.md) | Licenciamento sob GPL-3.0-or-later | Aceito |
| [0007](0007-toolchain-pinada.md) | Toolchain Rust pinada e disciplina de dependências | Aceito |

## Convenção

- Numeração sequencial de quatro dígitos, nunca reaproveitada.
- Nome do arquivo: `NNNN-titulo-em-kebab-case.md`.
- Status possíveis: `Proposto`, `Aceito`, `Rejeitado`, `Superseded by ADR-NNNN`.
- Decisão aceita não se edita. Para mudar, escreva um ADR novo com `Supersedes: ADR-NNNN` e marque o antigo. Correção de erro factual ou de clareza no texto é permitida; mudança de decisão, não. Revisão parcial de uma seção entra como blockquote de revisão na própria seção, apontando o ADR novo.
- Ao adicionar um ADR, atualize a tabela acima e, se a decisão for de stack, a lista "Stack travada" do [CLAUDE.md](../../CLAUDE.md).
- Decisão sobre o que trafega no fio (envelope, códigos, limites mínimos) não nasce aqui: nasce num ADR de [porecatu/protocol](https://github.com/porecatu/protocol), e o ADR deste repositório decide só como o relay a implementa.

---

## Template

```markdown
# ADR-NNNN — Título

**Status:** Proposto | Aceito | Rejeitado | Superseded by ADR-MMMM
**Data:** AAAA-MM-DD
**Relacionados:** ADR-XXXX, PRD-YYY

## Contexto

Qual é a força em jogo. O que torna esta decisão necessária agora, e o que
acontece se ela não for tomada. Requisitos e restrições concretos, não
generalidades.

## Decisão

O que foi decidido, em voz ativa e no presente. Uma frase, se possível.
Depois, os detalhes que a tornam acionável.

## Alternativas consideradas

Para cada alternativa séria: o que era, por que era plausível, e o motivo
específico de não ter sido escolhida. Alternativa sem motivo de rejeição
não é alternativa considerada, é enfeite.

## Consequências

### Positivas
### Negativas
### Riscos e mitigação

| Risco | Probabilidade | Impacto | Mitigação |
|---|---|---|---|
```
