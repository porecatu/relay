# ADR-0003 — Registro do host por chave e token do operador

**Status:** Aceito
**Data:** 2026-11-15
**Relacionados:** [ADR-0002](0002-relay-cego-e-sem-estado-persistente.md), [PRD-001](../prd/prd-001-operacao-do-relay.md), [especificação: transporte e relay §2 e §3](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md)

## Contexto

O relay não tem contas ([PRD-000](../prd/prd-000-visao-de-produto.md)), mas precisa responder a duas perguntas:

1. **Quem pode usar este relay?** Um relay auto-hospedado aberto vira, em pouco tempo, infraestrutura gratuita para terceiros.
2. **Quem é o dono de um endereço?** O cliente chega pedindo um `host_id`. Se qualquer um pudesse registrar qualquer `host_id`, um atacante tomaria o endereço de um desktop alheio. A sessão Noise impediria que ele lesse algo (o cliente confere a chave estática do host), mas o desktop legítimo ficaria fora do ar.

A especificação do protocolo já decidiu a forma ([§2 e §3](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md)): token do operador no cabeçalho `Authorization` do upgrade e desafio assinado com a chave de roteamento Ed25519 do host. Este ADR decide como o relay a implementa.

## Decisão

**A pergunta 1 é respondida pelo token do operador, conferido antes do upgrade; a pergunta 2, pelo desafio Ed25519, conferido depois. Os dois são independentes: o token diz que o host pode usar o relay; a chave diz qual endereço é dele.**

### 1. Token do operador

- Os tokens aceitos vêm de `[[tokens]]` no `relay.toml`, cada um com `name` e `value` (ou `value_file`). São carregados uma vez, na subida.
- Em `GET /v0/host`, **antes** do upgrade de WebSocket, o relay lê `Authorization: Bearer <token>` e compara com **todos** os tokens configurados com `subtle::ConstantTimeEq`, acumulando o resultado sem curto-circuito. Comprimentos diferentes também não podem vazar por tempo: a comparação é sobre o resumo BLAKE2s-256 de cada valor, que tem tamanho fixo.
- Ausente ou sem correspondência: **HTTP 401**, sem corpo, sem upgrade. O log registra a recusa com o IP (pela política do `[log] ip_addresses`) e nada do cabeçalho.
- O **nome** do token que casou acompanha a conexão do host até o fim e aparece nos logs dela.
- `/v0/client/{host_id}` não exige token. A autenticação do cliente é feita pelo host, na sessão Noise.

### 2. Desafio da chave de roteamento

Logo depois do upgrade em `/v0/host`:

1. O relay manda `challenge` com 32 bytes de `OsRng`.
2. Espera `register` por até `[limits] register_timeout_seconds` (default 10). Atraso: fecha com `4005`. Envelope de outro tipo ou CBOR inválido: fecha com `4004`.
3. Verifica `signature` sobre `"porecatu-relay/v0/register" || nonce` com `ed25519-dalek` em modo **estrito** (`verify_strict`, que recusa chaves de ordem pequena e assinaturas maleáveis). Falha: fecha com `4001`.
4. Calcula `host_id` pela fórmula da especificação, **pela função do crate `porecatu-protocol`**, nunca reimplementada aqui.
5. Responde `registered { host_id }` e põe o host no registro em memória.

### 3. A última registração válida vence

Se o `host_id` já tem conexão viva, a antiga é fechada com `4002` e cada cliente ligado a ela recebe `closed { host-replaced }` antes de ter o WebSocket fechado. Os clientes não migram para a conexão nova: a sessão Noise deles era com a conexão velha e não tem como continuar; eles reconectam e fazem handshake de novo. A troca no registro é atômica: não existe instante em que um cliente novo seja ligado à conexão velha.

É o comportamento de um notebook que trocou de Wi-Fi: a conexão velha ainda não caiu por timeout quando a nova chega. Como só quem tem a chave privada passa pelo desafio, "o último vence" não abre sequestro.

### 4. Token revogado

Revogar é remover do arquivo e reiniciar ([PRD-001](../prd/prd-001-operacao-do-relay.md) RF-1.8). Como o reinício derruba tudo, não há "host registrado com token que não existe mais".

## Alternativas consideradas

- **Relay aberto, sem token.** Mais simples para o operador. Rejeitada: relay aberto na internet é encontrado e usado por terceiros; o custo de banda e o risco legal ficam com o operador.
- **Token também para o cliente.** Fecharia o relay a quem não tem o token. Rejeitada: o token teria de ir no QR do pareamento, e um celular com o token pode registrar hosts. O cliente sem token só consegue chegar a `host_id` já registrados, e o host recusa quem não está pareado.
- **Registrar o `host_id` direto, sem desafio** (o token bastaria). Rejeitada: qualquer portador de token tomaria o endereço de outro host do mesmo relay — o "outro usuário do mesmo relay" do modelo de ameaça.
- **Desafio com a chave estática X25519 do Noise** em vez de uma Ed25519 separada. Evitaria uma chave a mais. Rejeitada pela especificação: X25519 não assina, e provar posse exigiria um mini-handshake Noise com o relay, que passaria a ter estado de sessão.
- **Primeira registração vence** (recusar a segunda enquanto a primeira vive). Rejeitada: o desktop que trocou de rede ficaria fora do ar até a conexão velha expirar (até 60 s).

## Consequências

### Positivas

- O operador controla quem usa o relay com uma lista de tokens nomeados.
- Endereço de host não pode ser tomado sem a chave privada, nem por quem tem token.
- O relay continua sem contas e sem estado em disco.

### Negativas

- Um token vale para um número qualquer de hosts: o operador que dá o mesmo token a dois desktops só os distingue pelo `host_id` no log.
- Revogar um token exige reinício, que derruba os outros hosts também.

### Riscos e mitigação

| Risco | Probabilidade | Impacto | Mitigação |
|---|---|---|---|
| Ataque de tempo na comparação do token | baixa | médio | Comparação de resumos de tamanho fixo, em tempo constante, sem curto-circuito |
| Token vaza em log de proxy reverso (cabeçalho `Authorization`) | média | médio | Documentado no guia de operação: proxy não deve logar o cabeçalho; tokens nomeados e revogáveis |
| Força bruta de tokens | baixa | médio | Tokens de 32+ caracteres aleatórios; limite de tentativas por IP (`[limits] host_attempts_per_ip_per_minute`) |
| Inundação de registros válidos alternando (dois processos com a mesma chave) | baixa | baixo | Comportamento definido (o último vence); o log mostra a troca em sequência |
