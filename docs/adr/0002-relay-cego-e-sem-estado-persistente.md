# ADR-0002 — Relay cego e sem estado persistente

**Status:** Aceito
**Data:** 2026-11-15
**Relacionados:** [ADR-0003](0003-registro-do-host-por-chave-e-token-do-operador.md), [PRD-000](../prd/prd-000-visao-de-produto.md), [PRD-001](../prd/prd-001-operacao-do-relay.md), [modelo de ameaça](https://github.com/porecatu/protocol/blob/main/docs/especificacao/modelo-de-ameaca.md)

## Contexto

Pelo relay passa o terminal inteiro do usuário: teclas, senhas digitadas, saída de programas, nomes de projetos e de servidores. O [modelo de ameaça do protocolo](https://github.com/porecatu/protocol/blob/main/docs/especificacao/modelo-de-ameaca.md) trata o relay como **hostil para conteúdo**: a sessão entre desktop e celular é Noise ponta a ponta, e o relay só vê o envelope ([especificação §7](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md)).

Isso protege contra um operador mal-intencionado, mas não basta contra um relay **descuidado**: um relay que guarda quadros em disco para depuração, que loga o `payload` em nível `debug`, ou que mantém fila de mensagens para cliente offline acumula exatamente o que um invasor procuraria — mesmo cifrado, é metadado farto e alvo de ataque futuro à cifra. A decisão de ser cego tem de chegar ao código, não só à criptografia.

## Decisão

**O relay não interpreta, não guarda e não registra nada acima do envelope, e não tem estado que sobreviva ao processo.**

1. **`payload` é opaco.** O tipo que o carrega no código (`OpaquePayload`, envolvendo o `Payload` do `porecatu-protocol`, que guarda `Bytes`) **não implementa `Debug`, `Display` nem `Serialize`**. Logar um `payload` não compila. A única operação sobre ele é encaminhar e medir o comprimento.

   > **Nota de implementação (2026-11-16):** `OpaquePayload` (`src/payload.rs`) também não implementa `Clone`: o relay entrega cada quadro a um destinatário só, e sem `Clone` uma cópia acidental ("guardada para depuração") não compila.
2. **Nada em disco.** Sem banco, sem arquivo de estado, sem cache, sem fila de mensagens para cliente ausente. O que o relay sabe — hosts registrados, canais abertos — vive em memória e morre com o processo.
3. **Sem fila para quem não está.** Cliente que chega para um host que não está registrado recebe `host-unavailable` e a conexão fecha ([especificação §4](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md)). Não há "guardar até o host voltar".
4. **Logs com campos fechados.** O que pode aparecer num log está listado no [PRD-001](../prd/prd-001-operacao-do-relay.md) RF-1.18 e em nenhum outro lugar; um teste roda uma sessão com marcador no `payload` e reprova se ele aparecer na saída em nível `trace`.
5. **Sem métricas por identidade.** Métricas, quando ligadas, são contagens agregadas, sem `host_id`, IP ou nome de token em rótulo.
6. **Reinício zera tudo**, e é o comportamento certo: host e cliente reconectam, e o registro do host é refeito pelo desafio ([ADR-0003](0003-registro-do-host-por-chave-e-token-do-operador.md)).

## Alternativas consideradas

- **Fila de mensagens para cliente offline** (o host manda, o relay entrega quando o celular voltar). Útil para notificação de "processo terminou". Rejeitada: transforma o relay num depósito de dados cifrados de longa duração, e notificação é exatamente o que ficou fora do v0 (push).
- **Persistir os registros de host** para sobreviver a reinício sem o host refazer o desafio. Rejeitada: o desafio custa um round-trip, e persistir o mapa `host_id → chave de roteamento` é guardar uma lista de quem usa o relay.
- **Log de depuração com `payload` atrás de uma opção.** Plausível para diagnosticar problema de protocolo. Rejeitada: opção que existe é opção que alguém liga em produção. Problema de protocolo se diagnostica no host ou no cliente, que têm as chaves e o texto claro; o relay contribui com tamanhos e momentos.
- **Relay que entende as mensagens de aplicação** (para compressão, cache de grade, multiplexar vários celulares num só fluxo do host). Rejeitada: exige as chaves, desfaz o modelo de ameaça.

## Consequências

### Positivas

- Comprometer o servidor do relay não entrega conteúdo de sessão nenhum, presente ou passado.
- Operação trivial: não há backup, migração nem limpeza de dados.
- O relay pode ser reiniciado ou trocado de máquina a qualquer momento.

### Negativas

- Toda reconexão refaz o registro e o handshake Noise; não há retomada de sessão.
- Diagnóstico do lado do relay fica limitado a tamanhos, momentos e motivos de fechamento.
- Recursos que dependeriam de estado no servidor (notificação para celular offline, histórico) ficam impossíveis neste desenho, não só adiados.

### Riscos e mitigação

| Risco | Probabilidade | Impacto | Mitigação |
|---|---|---|---|
| Alguém adiciona `#[derive(Debug)]` ao tipo do `payload` | média | alto | Teste de compilação negativa (`trybuild`) que exige que formatar o tipo não compile; o teste de log do RF-1.18 |
| Biblioteca de terceiros loga o quadro (ex.: `tracing` de dependência em `trace`) | média | alto | O filtro de `tracing` desliga os alvos das dependências de rede abaixo de `info`; o teste de log roda com o filtro de produção e com `trace` global |
| Metadados (tamanho, ritmo) revelam atividade | alta | médio | Aceito no modelo de ameaça; o relay é auto-hospedado |

> **Nota de implementação (2026-11-16):** o teste de compilação negativa não usa `trybuild`: são cinco exemplos `compile_fail` na documentação de `src/payload.rs` (`{:?}`, `{}`, `tracing` com `?payload`, `Serialize` e `clone()`), que o `cargo test` roda, mais um exemplo que compila para provar que a falha é do formato e não de digitação. O teste de log (`tests/blind_log.rs`) roda com o filtro de produção em `trace`, em texto e em JSON; os alvos das dependências de rede ficam em `info` no máximo e `RUST_LOG` é ignorado (`src/logging.rs`).
