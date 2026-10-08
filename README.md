<p align="center">
  <img src="assets/logo_mini.png" alt="SaguTerm" width="112">
</p>

<h1 align="center">SaguTerm</h1>

<p align="center">
  <strong>SSH, SFTP e terminal local num só lugar,<br>
  com todos os seus acessos protegidos num cofre criptografado.</strong>
</p>

<p align="center">
  <a href="https://apps.microsoft.com/detail/9N4S9R1BS9SM"><img alt="Microsoft Store" src="https://img.shields.io/badge/Microsoft%20Store-SaguTerm-6d28d9"></a>
  <img alt="Windows 10/11" src="https://img.shields.io/badge/Windows-10%20%7C%2011-0078d4">
  <img alt="Feito em Rust" src="https://img.shields.io/badge/feito%20em-Rust-b7410e">
  <a href="LICENSE"><img alt="Licença MIT" src="https://img.shields.io/badge/licen%C3%A7a-MIT-green"></a>
</p>

<p align="center">
  <a href="https://apps.microsoft.com/detail/9N4S9R1BS9SM"><strong>⬇️ Baixar na Microsoft Store</strong></a>
  &nbsp;·&nbsp;
  <a href="#comecar">Comece em 1 minuto</a>
  &nbsp;·&nbsp;
  <a href="#atalhos">Atalhos</a>
</p>

<!--
  Dica: uma captura de tela aqui aumenta muito o interesse de quem visita.
  Salve em docs/screenshot.png (com paineis divididos, de preferencia sem IPs
  reais a mostra) e troque este comentario por:
  <p align="center"><img src="docs/screenshot.png" alt="SaguTerm em uso" width="900"></p>
-->

---

Se você administra servidores, conhece a rotina: um programa para SSH, outro para
transferir arquivos, senhas espalhadas em anotações e uma janela para cada servidor.
O **SaguTerm** junta tudo isso numa janela só, rápida e feita para ser usada pelo teclado.

## ✨ Por que experimentar

- **🛍️ Direto da Microsoft Store.** Instale com um clique. As versões novas chegam pela
  própria Store.
- **🔐 Suas senhas não ficam soltas.** Hosts, usuários, senhas e chaves privadas ficam num
  arquivo `.sagu` criptografado com **AES-256-GCM**, com a chave derivada da sua senha mestra
  por **Argon2id**. Leve o arquivo para outra máquina e abra com a mesma senha.
- **🪟 Vários servidores na mesma tela.** Divida a janela em quantos painéis quiser, lado a
  lado ou empilhados, no estilo do tmux, e misture sessões SSH, SFTP e terminais locais.
- **⌨️ Feito para o teclado.** Digite parte do nome, aperte Enter e você está conectado.
  Troque de painel com `Alt+setas`. O mouse é opcional.
- **📂 SFTP sem outro programa.** Navegue pelas pastas do servidor, leia arquivos de texto
  na hora com `Enter`, arraste arquivos do Windows para enviá-los, baixe arquivos e pastas
  com `Ctrl+S` e copie ou mova com `Ctrl+C`/`Ctrl+X` e `Ctrl+V`.
- **🛡️ Sabe com quem está falando.** Na primeira conexão, o SaguTerm mostra a impressão
  digital da chave do servidor e pede a sua confirmação. Se a chave mudar depois, ele avisa
  antes de enviar qualquer senha.
- **⚡ Leve e rápido.** Escrito em Rust do começo ao fim, incluindo o SSH (sem OpenSSL),
  com interface acelerada pela placa de vídeo.

## 🧰 Funcionalidades

### Terminal SSH
- Emulação xterm com 256 cores, que se ajusta sozinha ao tamanho do painel.
- Programas de tela cheia como o **htop** e o **btop** aparecem no lugar certo, inclusive
  os gráficos do btop, feitos com caracteres braille, que o SaguTerm desenha ponto a ponto.
- **Emojis coloridos** (🟢 🟠 🚀 ✅ ❤️), desenhados pelo Windows com as mesmas imagens do
  seletor `Win + .`. Cada emoji ocupa duas colunas, como no servidor; os compostos
  (família, tom de pele) aparecem como emojis separados em vez de combinados. Símbolos
  que a fonte do terminal não tem (✓, ✗, 🛢, ideogramas) vêm das fontes do Windows em
  vez de um quadradinho.
- **Selecionou, copiou:** o texto selecionado vai direto para a área de transferência.
  O **botão direito cola**.
- **Histórico de rolagem:** o que já passou pela tela fica guardado (até 5.000 linhas da
  tela por painel; uma linha longa, quebrada em várias, conta como várias). Role com a
  **roda do mouse** ou com `Shift+PgUp`/`Shift+PgDn`; `Shift+Home` vai ao início e
  `Shift+End` volta ao fim (digitar qualquer coisa no terminal também volta). Enquanto você
  lê, a saída nova não puxa a tela, e um aviso no canto mostra quantas linhas acima você
  está. A seleção funciona no histórico e copia até o que não cabe na tela (arraste além da
  borda). O comando `clear` limpa o histórico; o `Ctrl+L` só limpa a tela (o que estava
  nela não vai para o histórico).
- Programas de tela cheia (tmux, less, htop, vim) não deixam histórico no SaguTerm. Se o
  programa usa o mouse (htop, mc, tmux com `set -g mouse on`), a roda vai para ele. No tmux
  sem mouse, use `Ctrl+B, Ctrl+B, [` (o primeiro `Ctrl+B` é do SaguTerm e envia o segundo
  ao tmux), role com `PgUp`/setas e saia com `q`; no less e no man, use `PgUp`/`PgDn` ou as
  setas. `Shift+roda` força a rolagem do SaguTerm.
- Autenticação por **senha** ou **chave privada** (formato OpenSSH ou PEM, com passphrase
  opcional).
- **Verificação da chave do servidor**, no SSH e no SFTP: na primeira conexão, uma janela
  mostra o tipo da chave e a impressão digital SHA256 para você conferir, com
  **Confiar e conectar** ou **Cancelar**. A chave aceita fica guardada no cofre, e as
  próximas conexões seguem direto enquanto ela for a mesma.
- Se o servidor apresentar outra chave, a conexão para num **alerta vermelho** com a
  impressão digital guardada e a nova, antes de enviar usuário, senha ou chave. Cancelar é
  o padrão: a chave nova só é aceita com um clique em **Aceitar a nova chave e conectar**.
- Keepalive automático, para a sessão não cair quando fica parada.
- Ao digitar `exit`, o painel fecha. Se a conexão cair, o painel fica aberto com o
  aviso, para você saber o que aconteceu.

### Terminal local
- **Prompt de Comando** e **WSL** (quando instalado) nos mesmos painéis dos servidores
  remotos, lado a lado.

### Painéis divididos
- Divida qualquer painel quantas vezes quiser: `Ctrl+B, H` (lado a lado) ou
  `Ctrl+B, V` (empilhado).
- Cada painel novo já abre o seletor de conexões com a busca pronta para digitar.
- Navegue com `Alt+setas`. O painel ativo fica destacado.

### Navegador SFTP
- Navegue pelas pastas com o teclado ou o mouse. As setas, `PageUp`/`PageDown` e
  `Home`/`End` movem na lista (com `Shift`, selecionam o intervalo). A linha `..`, no topo,
  recebe o cursor como os outros itens e sobe para a pasta acima com `Enter` ou duplo
  clique; o `Backspace` também sobe. Ao voltar, a pasta de onde você saiu fica selecionada.
- **Busca por letras:** digite as primeiras letras de um nome para ir direto a ele, sem
  diferenciar maiúsculas nem acentos. Repetir a mesma letra passa para o próximo item que
  começa com ela, e o que foi digitado aparece num indicador no canto da lista. Depois de
  1 segundo sem digitar, a busca recomeça.
- **Digite o caminho:** clique em qualquer ponto da barra do caminho (ou use `Ctrl+L`) e
  digite outro, com o texto atual já selecionado. Vale caminho absoluto, relativo à pasta
  atual ou com `~` para a sua pasta inicial. Se o caminho for de um arquivo, abre a pasta
  dele com o arquivo selecionado. Um caminho que não existe mostra o erro logo abaixo e
  deixa o texto para você corrigir; `Esc` cancela.
- **Pastas e arquivos inconfundíveis:** pastas em âmbar, com `/` no fim do nome e sempre no
  topo da lista. Links simbólicos têm uma seta no ícone e mostram o destino ao passar o
  mouse. Um link para pasta aparece junto das pastas e abre com `Enter`; um link quebrado
  aparece em vermelho; fifos, sockets e dispositivos aparecem em cinza.
- **Visualizador somente leitura:** `Enter` ou duplo clique num arquivo mostra o conteúdo
  dentro do próprio painel, em fonte monoespaçada, com números de linha, busca (`Ctrl+F`,
  `F3`), seleção e cópia. O cabeçalho mostra o tamanho, a codificação (UTF-8, UTF-16 com
  BOM ou Windows-1252, comum em arquivos antigos em português) e o fim de linha. `Esc`
  volta à listagem no mesmo item.
  - Arquivos binários não abrem: aparece um aviso (para usá-los, baixe com `Ctrl+S`).
    Fifos, sockets, dispositivos e links quebrados também não abrem, com um aviso.
  - De um arquivo com mais de 4 MiB aparece só o começo, com um aviso e o botão para
    baixá-lo inteiro. Acima de 200 mil linhas, o texto também é cortado, com aviso.
  - Caracteres de controle e de direção de texto (bidi) aparecem numa notação visível,
    como `^[` ou `<U+202E>`.
  - O conteúdo fica só na memória: nada é gravado em disco.
- **Arraste arquivos** do Windows para enviar à pasta atual. Um arquivo de mesmo nome
  nessa pasta é substituído.
- **Copie e mova** arquivos e pastas no servidor: selecione, tecle `Ctrl+C` (copiar) ou
  `Ctrl+X` (recortar), vá até a pasta de destino, no mesmo painel ou em outro painel SFTP
  da mesma conexão, e tecle `Ctrl+V` (ou use o botão da faixa no topo do painel, que
  mostra o que está sendo copiado ou movido; `Esc` desiste). Colar na mesma pasta cria uma
  cópia com "(cópia)" no nome. Nada é substituído sem perguntar, e mover para outro disco
  do servidor pede confirmação antes de copiar e apagar os originais. A cópia passa pelo
  SFTP, sem executar comandos no servidor; links simbólicos são copiados como links, com
  permissões e datas mantidas. Colar num painel de outro servidor ainda não é possível.
- **Baixe arquivos e pastas** para o computador: selecione os itens (`Ctrl+clique`,
  `Shift+clique` ou `Ctrl+A`), aperte `Ctrl+S` ou use o botão de download do cabeçalho
  e escolha a pasta de destino. Pastas vêm com todo o conteúdo, e o que não dá para
  baixar (como um link para outra pasta) aparece no resumo do fim.
- O andamento aparece no rodapé do painel, com **Cancelar**. Se algum item já existe no
  destino, você escolhe entre **Substituir**, **Pular existentes** ou **Cancelar**.
- Cada arquivo é baixado num temporário e só recebe o nome final quando termina, com a
  data de modificação do servidor. Nenhum arquivo pela metade fica com o nome final; se o
  app for fechado no meio do download, pode sobrar um temporário `.sagu-part` na pasta.
- Nomes que o Windows não aceita (como `CON`, `a:b` ou terminados em ponto) são ajustados,
  e nada é gravado fora da pasta que você escolheu.
- Renomeie, altere **permissões** (chmod), **proprietário/grupo** (chown) e exclua
  arquivos e pastas. Num link simbólico, permissões e proprietário valem para o destino, e
  excluir remove só o link, nunca o destino.

### Monitoramento
- O cartão **Monitoramento**, ao lado do Terminal local e do WSL, abre uma tela com um
  cartão por servidor cadastrado: anéis de **CPU** (média desde a coleta anterior),
  **memória** e do **disco** mais cheio, **load** de 1, 5 e 15 minutos, gráfico de CPU e
  memória da última hora, barras dos discos e do swap, tempo ligado e número de processos.
- Cada servidor ganha uma situação (**Saudável**, **Atenção** ou **Crítico**) pela pior
  medida, e o topo da tela resume quantos estão em cada uma. A dica do cartão lista o que
  está fora do normal e todos os discos. O load conta pelo menor entre os de 1 e 5
  minutos, por CPU: um pico curto e moderado não acusa, e a situação volta ao normal poucos
  minutos depois que a carga cai (sem esperar a média de 5 minutos baixar). CD ou ISO
  montado não conta como disco cheio, e um `df` que trava (compartilhamento de rede fora
  do ar) é encerrado em 5 segundos, mantendo os discos da leitura anterior.
- Atualiza sozinha a cada minuto enquanto estiver aberta (ou na hora, com **Atualizar
  agora**), usando uma conexão por servidor que fica aberta só enquanto a tela existir.
  Trocar o endereço, a porta, o usuário, a senha ou a chave de uma conexão, ou aceitar a
  chave do servidor no terminal, já dispara uma coleta. Se o servidor recusar as
  credenciais, o monitoramento não tenta de novo sozinho (para não acumular tentativas de
  login falhas no servidor): corrija a conexão ou clique em **Atualizar agora**.
- Funciona em servidores Linux (Windows e equipamentos de rede aparecem como **Não
  suportado**); a chave do servidor precisa ter sido confirmada antes pelo terminal, e
  conexões com **Detectar o sistema do servidor** desmarcado ficam de fora. O comando
  executado está na [política de privacidade](PRIVACY.md).

### Cofre e conexões
- Tela de conexões com **busca instantânea** pelo nome.
- Cadastre, edite e exclua hosts em qualquer seletor (`Ctrl+N` cria um novo).
- O cartão de cada servidor mostra o nome da conexão e o endereço (`usuário@host:porta`),
  com o ícone de **chave** ou de **senha** à esquerda do endereço, conforme o tipo de
  autenticação.
- **Dica completa:** ao passar o mouse sobre um cartão, a dica mostra o nome inteiro da
  conexão (mesmo quando ele não cabe no cartão), o endereço, o tipo de autenticação e, se
  já foi identificado, o sistema do servidor (por exemplo, "Sistema: AlmaLinux 8.10").
- **Ícone do sistema do servidor:** ao conectar pelo SSH ou pelo SFTP, o SaguTerm
  identifica em segundo plano o sistema do servidor e a versão dele (Ubuntu, Debian,
  Red Hat, CentOS, AlmaLinux, Rocky Linux, Fedora, openSUSE, Arch, Alpine, FreeBSD, macOS
  e outros) e passa a mostrar o ícone da distribuição no cartão da conexão. Nada aparece no
  terminal, e a sessão não espera por isso. Se o sistema não for identificado ou não tiver
  ícone no SaguTerm (como Oracle Linux, Amazon Linux e Kali Linux), o cartão mantém o
  ícone que já tinha (o de servidor, se o sistema nunca foi identificado). O sistema
  detectado fica guardado no cofre. A detecção pode ser desligada em cada conexão, no
  editor (**Detectar o sistema do servidor**).
- Ao editar um host, veja a impressão digital da chave do servidor guardada e, se
  precisar, use **Esquecer chave** para confirmá-la de novo na próxima conexão. Trocar o
  endereço ou a porta também apaga a chave guardada.
- Na tela de conexões, `Ctrl+L` **bloqueia o cofre** na hora.
- **Abrir sem senha neste computador:** marque a opção na tela de conexões e, nas próximas
  vezes, o SaguTerm abre o cofre direto ao iniciar, sem pedir a senha. A chave do cofre
  (nunca a senha) fica guardada protegida pela sua conta do Windows. Em outro computador ou
  em outra conta do Windows, o arquivo continua pedindo a senha. Bloquear o cofre faz a
  próxima abertura pedir a senha de novo.
- O cofre é salvo de forma atômica, então uma queda de energia no meio da gravação não
  corrompe o arquivo.
- **Uma janela só:** abrir o SaguTerm com ele já aberto (atalho de teclado, menu Iniciar,
  barra de tarefas) traz a janela existente para a frente, restaurando se estiver
  minimizada. Assim duas janelas nunca gravam o mesmo cofre, uma por cima da outra.

<a id="comecar"></a>

## 🚀 Comece em 1 minuto

1. **Instale o SaguTerm pela [Microsoft Store](https://apps.microsoft.com/detail/9N4S9R1BS9SM)**
   e abra-o pelo menu Iniciar.
2. **Crie o seu cofre:** escolha onde salvar o arquivo `.sagu` (de preferência numa pasta
   sua, como Documentos) e defina uma senha mestra.
3. **Cadastre um servidor** com `Ctrl+N` (nome, endereço, usuário e senha ou chave).
4. **Digite parte do nome e aperte Enter.**
5. **Na primeira conexão, confira a impressão digital** da chave do servidor e clique em
   **Confiar e conectar**. Pronto, você está conectado.

> 💡 A senha mestra não é guardada em lugar nenhum. Se você esquecê-la, não há como
> recuperar o cofre. Escolha uma senha forte e memorável.

<a id="atalhos"></a>

## ⌨️ Atalhos de teclado

A lista completa fica sempre à mão com **F1**, em qualquer tela (também em `Ctrl+B, A`).
No topo da ajuda aparece a versão instalada.

| Onde | Atalho | Ação |
|---|---|---|
| Painéis | `Alt+setas` | Trocar de painel na direção da seta |
| Painéis | `Ctrl+B, H` / `Ctrl+B, V` | Dividir lado a lado / empilhado |
| Painéis | `Ctrl+B, O` | Ciclar entre os painéis |
| Painéis | `Ctrl+B, X` | Fechar o painel ativo |
| Painéis | `Ctrl+B, Ctrl+B` | Enviar `Ctrl+B` ao terminal (útil com tmux remoto) |
| Painéis | `Ctrl+B, F1` | Enviar `F1` ao terminal (ex.: ajuda do htop ou do mc) |
| Terminal | Roda do mouse / `Shift+PgUp` / `Shift+PgDn` | Rolar o histórico (3 linhas por clique / uma página) |
| Terminal | `Shift+Home` / `Shift+End` | Início / fim do histórico (digitar também volta ao fim) |
| Terminal | `Shift+roda` | Rolar o histórico mesmo quando o programa usa o mouse |
| Terminal | `Ctrl+B, Ctrl+B, [` | Rolar dentro do tmux (modo cópia; `q` sai) |
| Conexões | *digitar* | Filtrar pelo nome |
| Conexões | `Enter` / `Ctrl+Enter` | Conectar via SSH / abrir SFTP |
| Conexões | `Ctrl+N` | Cadastrar novo host |
| Conexões | `Ctrl+L` | Bloquear o cofre |
| SFTP | `Enter` / duplo clique | Abrir a pasta ou visualizar o arquivo (na linha `..`, subir) |
| SFTP | `Backspace` | Voltar à pasta acima |
| SFTP | setas / `PageUp` / `PageDown` / `Home` / `End` | Mover na lista (com `Shift`, selecionar o intervalo) |
| SFTP | *digitar letras* | Ir ao item cujo nome começa com elas |
| SFTP | `Ctrl+L` / clique na barra do caminho | Digitar outro caminho (`~` é a pasta inicial) |
| SFTP | `Ctrl+clique` / `Shift+clique` / `Ctrl+A` | Marcar itens / selecionar um intervalo / selecionar tudo |
| SFTP | `Ctrl+S` | Baixar a seleção para o computador |
| SFTP | `Ctrl+C` / `Ctrl+X` / `Ctrl+V` | Copiar / recortar / colar arquivos e pastas (mesma conexão) |
| SFTP | `Esc` | Desistir de copiar ou mover |
| SFTP | `F2` / `Delete` / `F5` | Renomear / excluir / atualizar |
| Visualizador | `Ctrl+F` / `F3` / `Shift+F3` | Buscar / próxima ocorrência / ocorrência anterior |
| Visualizador | `Tab` | Alternar entre o campo da busca e o texto |
| Visualizador | `Ctrl+A` / `Ctrl+C` | Selecionar tudo / copiar (arrastar o mouse também copia) |
| Visualizador | `Ctrl+S` / `F5` | Baixar o arquivo / recarregar |
| Visualizador | `Esc` | Fechar a busca / voltar à listagem |

## 🔐 Segurança

- **Cofre:** AES-256-GCM, com chave de 256 bits derivada da senha mestra por Argon2id e
  *salt* aleatório. O arquivo é autossuficiente e portátil. Não abra o cofre nas versões
  antigas 0.1.x: elas não conferem a chave do servidor e, ao salvar qualquer alteração,
  apagam as chaves guardadas. De volta a esta versão, cada servidor apareceria como novo.
- **Na memória:** a senha mestra é descartada assim que o cofre abre. A chave derivada
  fica na memória enquanto o cofre está aberto, para gravar as alterações, e é apagada ao
  bloquear.
- **Abrir sem senha neste computador:** a chave do cofre fica guardada com a DPAPI do
  Windows, que só a devolve à sua conta do Windows, neste computador. Com a opção ligada,
  a proteção do cofre passa a ser a da sua conta do Windows: use senha ou PIN no Windows e
  bloqueie a tela (`Win+L`) ao se afastar.
- **Chave do servidor:** o SaguTerm confere a chave de cada servidor no estilo do PuTTY
  (*trust on first use*). Na primeira conexão, você confere a impressão digital SHA256 e
  decide se confia. A chave aceita fica guardada no cofre criptografado, e não no
  `known_hosts` do OpenSSH. Se o servidor apresentar outra chave depois, a conexão para
  num alerta antes de enviar usuário, senha ou chave, e só segue se você aceitar a chave
  nova com um clique. Cancelar (ou fechar o painel) encerra a conexão sem gravar nada.
- **Sistema do servidor:** para escolher o ícone do cartão, o SaguTerm executa no servidor,
  com o seu usuário e num canal separado da mesma conexão, um comando fixo que só lê a
  identificação do sistema (`uname` e arquivos como `/etc/os-release`). Ele não roda em
  servidores que se identificam, no início da conexão SSH, como Windows ou como alguns
  equipamentos de rede. O que o servidor devolve é tratado como não confiável: o app guarda
  só o identificador, o nome e a versão do sistema, com tamanho limitado e sem caracteres
  de controle. Se o servidor recusar o comando ou demorar, o app desiste sem avisar. Se o
  servidor força um comando (`ForceCommand` no `sshd_config` ou `command="..."` no
  `authorized_keys`), é esse comando que roda no lugar do comando de detecção, uma vez a
  mais a cada detecção, e o `~/.ssh/rc` e o `/etc/ssh/sshrc`, se existirem, também rodam
  de novo.
  Nesses servidores, desligue a detecção no editor da conexão. O comando exato está na
  [política de privacidade](PRIVACY.md).
- **Downloads:** os nomes que vêm do servidor são conferidos antes de gravar. Nomes com
  `..`, barras, caracteres proibidos no Windows, nomes reservados (`CON`, `NUL`...) ou
  fluxos alternativos do NTFS são ajustados ou ignorados, e nada sai da pasta de destino.
  Nada que já existe é substituído sem a sua confirmação.
- **Nomes e arquivos vindos do servidor:** caracteres de controle e de direção de texto
  (bidi) nos nomes da listagem, nos destinos dos links e no visualizador aparecem numa
  notação visível, em vez de agir sobre a tela, e o texto copiado do visualizador vem na
  mesma notação. O visualizador confere o tipo antes de abrir e nunca abre fifos, sockets,
  dispositivos nem interfaces do kernel que bloqueiam ou consomem os dados ao ler (como o
  `/proc/kmsg`). Seguir os links da listagem, o caminho digitado na barra e as leituras do
  visualizador correm num canal SFTP à parte, na mesma conexão: se um destino travar o
  servidor (uma montagem de rede parada, por exemplo), só esse canal é descartado, e a
  navegação, os downloads e os envios continuam. Se o servidor não permitir o canal extra,
  tudo vai pelo canal principal.
- **Pacotes do servidor:** uma resposta SFTP acima de 1 MiB interrompe aquele canal SFTP (as
  operações seguintes nele falham até reconectar), em vez de o app reservar a memória que o
  servidor pedir.
- **Instalação e atualizações:** pela Microsoft Store, que entrega o pacote assinado. O app
  não tem atualizador próprio.
- **Privacidade:** sem conta, sem anúncios e sem telemetria. O app não envia nenhum dado ao
  desenvolvedor e não acessa a internet por conta própria: só se conecta aos servidores que
  você cadastra. Detalhes na [política de privacidade](PRIVACY.md).

## 🗺️ Próximos passos

- [x] Verificação da chave do servidor (guardada no cofre)
- [x] Download de arquivos e pastas pelo SFTP
- [x] Visualizador de arquivos de texto no navegador SFTP (somente leitura)
- [x] Copiar e mover arquivos e pastas no servidor (Ctrl+C / Ctrl+X / Ctrl+V)
- [x] Histórico de rolagem no terminal (roda do mouse e Shift+PgUp)
- [x] Assinatura digital (o pacote é assinado pela Microsoft Store)
- [ ] Enviar pastas inteiras (hoje o envio por arrastar aceita só arquivos)
- [ ] Arrastar arquivos do navegador SFTP direto para o Explorador de Arquivos
- [ ] Renomear e excluir vários itens de uma vez no navegador SFTP
- [ ] Copiar e mover entre servidores diferentes

Tem uma ideia ou encontrou um problema? [Abra uma issue](https://github.com/alowelter/sagu-term/issues).
Toda sugestão ajuda.

## 🛠️ Compilando a partir do código

Requisitos: [Rust](https://rustup.rs) estável e o **Visual Studio Build Tools**
(componente "Desenvolvimento para desktop com C++", que inclui o Windows SDK).

```powershell
git clone https://github.com/alowelter/sagu-term.git
cd sagu-term
cargo build --release
# executável em target\release\SaguTerm.exe
```

Para gerar o pacote MSIX da Microsoft Store, rode `packaging/msix/build.ps1` depois do
`cargo build --release`. O passo a passo está em [packaging/msix/LOJA.md](packaging/msix/LOJA.md).

## 🎨 Créditos

Os ícones da interface (os arquivos `.svg` de `assets/`, fora a pasta `assets/os/`) vêm do
projeto [Lucide](https://lucide.dev), distribuído sob a licença
[ISC](https://github.com/lucide-icons/lucide/blob/main/LICENSE) — Copyright (c) 2026 Lucide Icons
and Contributors. Parte deles deriva do [Feather](https://feathericons.com), sob a licença MIT —
Copyright (c) 2013-present Cole Bemis. A única alteração feita nos arquivos foi pintar o traço
de branco (`stroke="#ffffff"`), para que o SaguTerm aplique a cor do tema.

<details>
<summary>Textos das licenças ISC (Lucide) e MIT (Feather)</summary>

```text
ISC License

Copyright (c) 2026 Lucide Icons and Contributors

Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted, provided that the above
copyright notice and this permission notice appear in all copies.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
```

```text
The MIT License (MIT) (para os ícones derivados do Feather)

Copyright (c) 2013-present Cole Bemis

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

</details>

Os ícones de sistemas operacionais dos cartões de conexão vêm do projeto
[Simple Icons](https://simpleicons.org) (pacote `simple-icons`, versão 16.33.0). O Simple Icons
é distribuído sob a licença [CC0 1.0](https://github.com/simple-icons/simple-icons/blob/develop/LICENSE.md),
mas nem todo ícone do projeto é CC0: os que têm licença própria estão listados abaixo, e os
arquivos correspondentes em `assets/os/` continuam sob essas licenças. A única alteração feita
nos arquivos foi pintar o desenho de branco (`fill="#ffffff"`), para que o SaguTerm aplique a
cor de cada marca.

| Ícone | Autor ou titular | Licença do ícone | Origem |
| --- | --- | --- | --- |
| Debian (logotipo de uso livre) | © 1999 Software in the Public Interest, Inc. | [CC BY-SA 3.0](https://creativecommons.org/licenses/by-sa/3.0/) (o logotipo também é oferecido sob LGPL 3.0 ou posterior) | [debian.org/logos](https://www.debian.org/logos) |
| Devuan (emblema) | hellekin, com golinux e Centurion_Dan; publicado pela Dyne.org Foundation | [CC BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/) | [git.devuan.org](https://git.devuan.org/devuan/documentation/src/commit/f4931e70e17f043c2824d591bba7c7f70ed583b6/art/graphics/logo/devuan-emblem.svg) |
| Gentoo (logotipo "g", versão vetorial) | Lennart Andre Rolland e Gentoo Foundation, Inc. | [CC BY-SA 2.5](https://creativecommons.org/licenses/by-sa/2.5/) | [wiki.gentoo.org](https://wiki.gentoo.org/wiki/Project:Artwork/Artwork#Variations_of_the_.22g.22_logo) |
| NixOS (logotipo) | Simon Frankau, Tim Cuthbertson e Daniel Baker, mantido pela equipe de marketing do NixOS | [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/) | [NixOS/branding](https://github.com/NixOS/branding) ([brand.nixos.org](https://brand.nixos.org)) |
| Rocky Linux (ícone) | Rocky Enterprise Software Foundation (projeto Rocky Linux) | [CC BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/) | [rocky-linux/branding](https://github.com/rocky-linux/branding) |
| Fedora (logotipo) | Red Hat, Inc. (projeto Fedora) | [Diretrizes da marca Fedora](https://docs.fedoraproject.org/en-US/project/brand/) | [docs.fedoraproject.org](https://docs.fedoraproject.org/en-US/project/brand/) |

Os demais ícones (AlmaLinux, Alpine Linux, Apple, Arch Linux, CentOS, elementary, EndeavourOS,
FreeBSD, Linux Mint, Manjaro, openSUSE, OpenWrt, Pop!_OS, Raspberry Pi, Red Hat, Slackware, SUSE,
Ubuntu, Void Linux e Zorin) não têm licença específica registrada no Simple Icons.

Todos os nomes e logotipos são marcas de seus respectivos donos e aparecem aqui apenas para
identificar o sistema do servidor. O SaguTerm não tem vínculo com esses projetos e empresas
nem é endossado por eles. Entre outras: Debian é marca registrada da Software in the Public
Interest, Inc.; Devuan é marca registrada da Dyne.org Foundation; Gentoo é marca da Gentoo
Foundation, Inc. e do Förderverein Gentoo e.V.; Rocky Linux é marca da Rocky Enterprise
Software Foundation; Fedora e o logotipo do infinito são marcas da Red Hat, Inc.

## 📄 Licença

Distribuído sob a licença [MIT](LICENSE). Use, modifique e compartilhe à vontade.

<p align="center"><sub>Feito em Rust por <a href="https://github.com/alowelter">Marcelo Welter</a>.</sub></p>
