# PRD-000 — Visão de produto

**Status:** Aprovado
**Data:** 2026-11-15
**Requisito de origem:** pedido do dono do produto do Porecatu: acessar uma sessão viva do Porecatu desktop por um app mobile, através de um servidor intermediário. As decisões de arquitetura foram tomadas com ele antes da escrita e estão no [PRD-025 do Porecatu](https://github.com/porecatu/terminal/blob/main/docs/prd/prd-025-acesso-remoto.md) e na [especificação do protocolo](https://github.com/porecatu/protocol/blob/main/docs/especificacao/00-visao-geral.md).

> O dono decidiu, antes da escrita:
>
> - o relay é **cego**: só roteia bytes cifrados de ponta a ponta entre desktop e celular;
> - é **só auto-hospedado**: não há relay público oficial, nem contas;
> - é escrito em **Rust, com `tokio` e `axum`**;
> - é licenciado sob **GPL-3.0-or-later**, como o Porecatu.

## Problema

O Porecatu desktop não escuta porta nenhuma, e não deve: um emulador de terminal com porta aberta para a internet é um convite. O celular, por sua vez, quase nunca está na mesma rede que o desktop — está no 4G, no Wi-Fi de outro lugar, atrás de NAT de operadora. Os dois precisam de um ponto de encontro que ambos alcancem **saindo**, sem abrir nada.

Esse ponto de encontro vê passar o terminal inteiro do usuário: o que ele digita (inclusive senhas), o que os programas imprimem, o nome dos projetos e dos servidores em que trabalha. Um intermediário que pudesse ler isso seria o lugar mais valioso para atacar.

## Proposta

Um servidor pequeno, que o próprio usuário roda, e que **não consegue ler** o que encaminha:

1. O desktop se conecta ao relay e se registra com um endereço derivado de uma chave dele (`host_id`).
2. O celular se conecta ao relay pedindo aquele `host_id`.
3. O relay liga as duas conexões e passa bytes de uma para a outra. A sessão entre desktop e celular é cifrada de ponta a ponta (Noise); o relay nunca tem as chaves.

## Usuário-alvo

**Quem já roda o próprio servidor**: um VPS, um homelab, um Raspberry Pi atrás de um proxy reverso, um contêiner num cluster de casa. Sabe apontar um domínio, emitir um certificado (ou já tem um proxy que faz isso) e editar um TOML. É o mesmo usuário do Porecatu — desenvolvedor, confortável com arquivo de configuração — no papel de operador.

**Não é para:** quem quer acesso remoto sem operar nada. Para esse, o Porecatu não tem resposta no v0; ver "Não-objetivos".

## Princípios

1. **Cego por construção.** O relay não tem chave nenhuma da sessão e não interpreta nada acima do envelope ([ADR-0002](../adr/0002-relay-cego-e-sem-estado-persistente.md)).
2. **Nada em disco.** Sem banco, sem arquivo de estado, sem cache. Reiniciar é zerar.
3. **Logs sem conteúdo.** Só eventos de conexão e contagens.
4. **Operação simples.** Um binário, um arquivo de configuração, um token por desktop. Sem dependência externa (banco, fila, Redis).
5. **Falha alto.** Limite estourado fecha o canal com motivo; nunca descarta dado em silêncio, porque a sessão cifrada não sobrevive a um registro perdido.
6. **A especificação manda.** O relay implementa as camadas 1 e 2 do [protocolo](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md), sem extensão própria.

## Escopo do v0

| # | Recurso | Onde |
|---|---|---|
| 1 | Registro do host por token do operador e desafio Ed25519 | [ADR-0003](../adr/0003-registro-do-host-por-chave-e-token-do-operador.md) |
| 2 | Roteamento de canais entre clientes e host, em ordem | [arquitetura](../arquitetura.md) |
| 3 | TLS embutido ou atrás de proxy reverso | [ADR-0004](../adr/0004-tls-embutido-ou-atras-de-proxy.md) |
| 4 | Operação: configuração, tokens, limites, logs, saúde, encerramento | [PRD-001](prd-001-operacao-do-relay.md) |
| 5 | Binário por plataforma e imagem Docker | [ADR-0005](../adr/0005-empacotamento-binario-e-docker.md) |

## Não-objetivos do v0

Cada um destes é decisão, não esquecimento.

- **Relay público oficial.** Custo de operação, abuso, limites por usuário e obrigação de privacidade sobre metadados. Decisão do dono.
- **Contas de usuário.** Identidade é chave; autorização de registro é token do operador.
- **Persistência.** Nem de sessões, nem de registros, nem de mensagens para cliente offline. Desktop desligado é desktop offline.
- **Notificação push** (APNs/FCM). Exigiria credenciais da Apple e do Google no servidor e um payload cego próprio. Fora do v0 do app também.
- **Painel web de administração.** O operador edita o TOML e lê o log.
- **Federação ou vários relays cooperando.** Um host fala com um relay.
- **Conexão direta entre desktop e celular** (sem relay, na mesma rede). É assunto do protocolo, não deste projeto.

## Métricas de sucesso

| Métrica | Alvo |
|---|---|
| Latência acrescentada pelo relay (mesmo datacenter que os dois lados) | < 2 ms no p99 |
| Memória por canal ocioso | < 64 KiB |
| Memória do processo ocioso, sem hosts | < 20 MiB |
| Tempo do zero ao primeiro host registrado, para quem já tem domínio e certificado | < 15 minutos seguindo o README |
| Bytes de conteúdo em log | **zero** |

A última é a que define o produto. Se um único `payload` aparecer num log, o relay falhou no que se propôs.

## Riscos de produto

| Risco | Mitigação |
|---|---|
| Usuário sem servidor fica sem acesso remoto | Aceito: é decisão do dono. O README mostra o caminho mais curto (contêiner atrás de proxy existente) |
| Operador expõe o relay sem TLS | O relay recusa escutar sem TLS fora de loopback, salvo `behind_proxy = true` explícito ([ADR-0004](../adr/0004-tls-embutido-ou-atras-de-proxy.md)) |
| Token vazado permite a terceiros usar o relay | Tokens nomeados, revogáveis um a um; o token não dá acesso a conteúdo de ninguém |
| Metadados (quem, quando, quanto) ficam visíveis ao operador | Aceito no [modelo de ameaça](https://github.com/porecatu/protocol/blob/main/docs/especificacao/modelo-de-ameaca.md); o operador costuma ser o próprio usuário |
