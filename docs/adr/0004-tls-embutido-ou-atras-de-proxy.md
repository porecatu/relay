# ADR-0004 — TLS embutido ou atrás de proxy reverso

**Status:** Aceito
**Data:** 2026-11-15
**Relacionados:** [ADR-0001](0001-rust-tokio-e-axum.md), [ADR-0005](0005-empacotamento-binario-e-docker.md), [PRD-001](../prd/prd-001-operacao-do-relay.md), [especificação: transporte §1](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md)

## Contexto

A especificação exige `wss://` entre host/cliente e relay, com TLS 1.2 ou superior, e recusa `ws://` fora de `localhost` ([§1](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md)). O conteúdo já está protegido pelo Noise; o TLS protege o **token do operador** no cabeçalho `Authorization` e esconde da rede o caminho (`/v0/client/<host_id>`).

O usuário-alvo ([PRD-000](../prd/prd-000-visao-de-produto.md)) chega de dois jeitos:

- com um **proxy reverso** que já termina TLS para outros serviços (Caddy, nginx, Traefik), e quer pôr o relay atrás dele;
- com um **VPS limpo**, onde o mais simples é o relay escutar na 443 com o certificado que o `certbot` emitiu.

## Decisão

**O relay suporta os dois modos: TLS embutido com `rustls`, a partir de arquivos de certificado e chave; ou HTTP simples atrás de um proxy reverso. Sem TLS, ele só escuta em loopback, a menos que `behind_proxy = true` seja dito explicitamente.**

### 1. TLS embutido

- Ligado quando `[tls] cert_file` e `[tls] key_file` estão presentes (PEM; cadeia completa no `cert_file`).
- `rustls` com provedor `ring`, via `axum-server`. Versão mínima por `[tls] min_version`, default `"1.3"`; `"1.2"` aceito, nada abaixo.
- ALPN `http/1.1` (WebSocket sobre HTTP/2 não é usado).
- **Recarga do certificado** sem reiniciar: o relay observa o `mtime` dos dois arquivos a cada `[tls] reload_check_seconds` (default 300) e, mudando, carrega o par novo numa configuração `rustls` nova para as conexões **seguintes**. Par inválido: mantém o anterior e loga o erro. Conexões abertas continuam com o certificado com que foram feitas.
- Sem ACME embutido: emitir e renovar é do `certbot`, do orquestrador ou de quem o operador já usa.

> **Nota de implementação (2026-11-16):** a recarga (`tls::watch`, `src/tls.rs`) compara o `mtime` **e o tamanho** de cada arquivo, carrega o par novo fora da thread do runtime (`spawn_blocking`) e o troca com `RustlsConfig::reload_from_config` do `axum-server`. `reload_check_seconds` aceita a partir de 1. O par é lido também na subida, antes de escutar: par ilegível impede a subida (código `2`). O provedor `ring` é passado explicitamente ao `rustls`, sem depender de um provedor padrão instalado por outro crate.

### 2. Atrás de proxy

- Sem `[tls]`, o relay fala HTTP/WebSocket simples.
- **Regra de subida:** sem TLS, o `bind` tem de ser de loopback (`127.0.0.0/8`, `::1`) **ou** `[server] behind_proxy = true`. Do contrário o processo não sobe, com a mensagem dizendo as duas saídas.
- `behind_proxy = true` com `bind` fora de loopback é o caso do contêiner, cuja rede interna não é loopback mas também não é a internet. A responsabilidade passa a ser do operador, e a chave tem esse nome para que ninguém a ligue sem saber o que diz.
- IP real do cliente por `X-Forwarded-For`, aceito só de endereços em `[server] trusted_proxies` ([PRD-001](../prd/prd-001-operacao-do-relay.md) RF-1.13).

  > **Nota (2026-11-16):** a regra completa, inclusive a decisão do dono para a cadeia em que todos os saltos são proxies confiáveis (vale o IP do socket), está no RF-1.13 do [PRD-001](../prd/prd-001-operacao-do-relay.md). O IP resolvido vale para os logs e para os limites por IP, e `trusted_proxies` é aceito com ou sem `behind_proxy`.
- O proxy **DEVE** repassar o upgrade de WebSocket e não deve impor timeout de inatividade menor que 60 s (o relay pinga a cada 25 s). Exemplos para Caddy e nginx entram no guia de operação na etapa R3.

## Alternativas consideradas

- **Só TLS embutido.** Mais seguro por padrão. Rejeitada: obriga quem já tem proxy reverso a dar uma porta e um certificado só para o relay, que é exatamente o atrito que tira o usuário-alvo do caminho.
- **Só atrás de proxy.** O relay ficaria menor e sem `rustls`. Rejeitada: o VPS limpo passaria a exigir instalar e configurar um proxy só para isso.
- **ACME embutido** (`rustls-acme`). Experiência de "um comando e funciona". Rejeitada no v0: precisa de porta 80 ou desafio TLS-ALPN, de estado em disco (conta e certificados) — o que fere o [ADR-0002](0002-relay-cego-e-sem-estado-persistente.md) — e é mais uma superfície. Pode voltar como ADR próprio, guardando o estado fora do processo.
- **Sem TLS em lugar nenhum, confiando no Noise.** O conteúdo estaria protegido. Rejeitada: o token do operador iria em claro, e a especificação proíbe `ws://` fora de `localhost`.
- **OpenSSL** (`native-tls`). Rejeitada: impede o binário estático e traz dependência de sistema.

## Consequências

### Positivas

- O operador escolhe o modo que combina com a infraestrutura que já tem.
- Esquecer o TLS não deixa o relay exposto em silêncio: ele não sobe.
- Renovação de certificado sem derrubar sessões.

### Negativas

- Dois caminhos de configuração para documentar e testar.
- `behind_proxy = true` é uma chave que desliga uma proteção; mal usada, expõe o token.

### Riscos e mitigação

| Risco | Probabilidade | Impacto | Mitigação |
|---|---|---|---|
| Operador liga `behind_proxy` com o relay exposto direto | baixa | médio | Nome explícito da chave; aviso no log na subida quando `behind_proxy = true` e o `bind` não é privado (RFC 1918/4193) |
| Proxy derruba WebSocket ocioso | média | baixo | Ping de 25 s do relay; exemplos de proxy com timeout adequado |
| Certificado expira porque a renovação externa falhou | média | alto | O relay loga aviso a 14 dias do vencimento, uma vez por dia |

> **Nota de implementação (2026-11-16):** duas mitigações desta tabela **não estão implementadas**:
>
> - **Aviso de vencimento do certificado a 14 dias.** Decidido pelo dono em 2026-11-16: fica como pendência registrada no [roadmap](../roadmap.md#pendências); até lá, vigiar o vencimento fica com o operador.
> - **Aviso na subida com `behind_proxy = true` e `bind` fora de rede privada.** O relay sobe sem aviso; também pendência no roadmap, sem decisão tomada.
