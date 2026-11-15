# ADR-0007 — Toolchain Rust pinada e disciplina de dependências

**Status:** Aceito
**Data:** 2026-11-15
**Relacionados:** [ADR-0001](0001-rust-tokio-e-axum.md), [ADR-0011 do Porecatu](https://github.com/porecatu/terminal/blob/main/docs/adr/0011-toolchain-rust.md)

## Contexto

O relay é um serviço de rede exposto à internet. Build que muda sozinho (toolchain flutuante, dependência que sobe sem ninguém ler o changelog) é risco de segurança, não só de reprodutibilidade. O Porecatu resolveu o mesmo problema no [ADR-0011](https://github.com/porecatu/terminal/blob/main/docs/adr/0011-toolchain-rust.md), e o relay compartilha com ele o crate `porecatu-protocol`, que precisa compilar nas duas toolchains.

## Decisão

**O relay adota a mesma disciplina do Porecatu: toolchain stable pinada por versão exata, edition 2024, lints do projeto como erro, e dependências sensíveis pinadas por versão exata.**

1. **`rust-toolchain.toml`** com a versão stable exata (não `stable`), com `rustfmt` e `clippy`. A versão acompanha a do Porecatu sempre que possível: os dois compilam o `porecatu-protocol`, e uma subida de toolchain que quebra um quebra o outro.
2. **`rust-version`** no `Cargo.toml` igual à da toolchain.
3. **Lints** em `[lints]`: `unsafe_code = "deny"`, `missing_docs` em itens públicos, e o conjunto de `clippy` do Porecatu (`pedantic` com as exceções dele). `cargo clippy --all-targets --all-features -- -D warnings` no CI.
4. **Versão exata** (`=x.y.z`) para `porecatu-protocol`, `axum`, `tokio-tungstenite` (transitivo do `axum`, conferido no `Cargo.lock`), `rustls` e `ed25519-dalek`. Subir qualquer um é tarefa própria, com changelog lido e os testes de integração do envelope rodando.
5. **`cargo deny`** (licenças, fontes, banidos) e **`cargo audit`** (RUSTSEC) no CI, reprovando.
6. **Actions do GitHub pinadas por SHA de commit**, com a versão no comentário; Dependabot atualiza SHA e comentário juntos.
7. CI nas três plataformas para `build`, `test`, `clippy` e `fmt`; os alvos `musl` só no job de release.

## Alternativas consideradas

- **`stable` flutuante.** Menos manutenção. Rejeitada: o build de uma tag antiga deixa de ser reprodutível, e um lint novo do `clippy` quebra o CI de um dia para o outro sem mudança no código.
- **MSRV baixo** para compilar em toolchains antigas de distribuição. Rejeitada: o relay é distribuído como binário; ninguém precisa compilá-lo com o Rust do sistema.

## Consequências

### Positivas

- Build reprodutível a partir de qualquer tag.
- Mudança de dependência sensível sempre passa por alguém.
- Mesmas regras do Porecatu: quem contribui num contribui no outro.

### Negativas

- Subir a toolchain é tarefa periódica, coordenada com o Porecatu.

### Riscos e mitigação

| Risco | Probabilidade | Impacto | Mitigação |
|---|---|---|---|
| Correção de segurança atrasada por versão exata | média | alto | `cargo audit` no CI agendado diariamente; subida de segurança passa na frente |
| Toolchain do relay e do Porecatu divergem e o `porecatu-protocol` não compila num dos dois | baixa | médio | O CI do protocolo roda nas duas versões enquanto elas diferirem |
