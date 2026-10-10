# Política de Privacidade do SaguTerm

**Vigência:** 10 de outubro de 2026 · [English version below](#english)

Esta política vale para o SaguTerm distribuído pela **Microsoft Store** e também para um
executável que você mesmo compile a partir do código-fonte, que é público no GitHub. O
SaguTerm é um cliente SSH/SFTP com terminal local para Windows, de código aberto (licença
MIT), mantido por Marcelo Welter ("o desenvolvedor").

## Resumo

- **O SaguTerm não envia nenhum dado ao desenvolvedor.** Não há conta, cadastro, anúncios,
  telemetria, estatísticas de uso nem relatórios de erro próprios, e o app não grava
  registros (logs) em disco.
- Suas conexões, senhas e chaves, as chaves públicas dos servidores que você aceitou e o
  sistema operacional detectado em cada servidor ficam num **cofre criptografado**, num
  arquivo guardado onde você escolher. Se você ligar **Abrir sem senha neste computador**,
  a chave do cofre fica guardada no seu computador, protegida pela sua conta do Windows
  (seção 1).
- Os arquivos que você baixa dos seus servidores vão só para a pasta que você escolhe.
- O visualizador de arquivos do navegador SFTP lê do servidor só o arquivo que você abre,
  para mostrá-lo na tela. O conteúdo fica na memória e não é gravado em disco.
- Copiar e mover arquivos no navegador SFTP (`Ctrl+C`/`Ctrl+X` e `Ctrl+V`) acontece no
  próprio servidor, e arrastar arquivos de um painel SFTP para outro os copia para o
  servidor desse painel, que pode ser outro dos que você cadastrou: os dados de uma cópia
  passam pela memória do app, sem serem gravados no seu computador.
- Ao conectar a um servidor, o app pode executar nele, com o seu usuário, um comando fixo
  que só lê a identificação do sistema operacional, para mostrar o ícone do sistema no
  cartão da conexão. Isso pode ser desligado em cada conexão (seções 2 e 4).
- Com a tela **Monitoramento** aberta, o app executa a cada minuto, em cada servidor
  cadastrado com a detecção do sistema ligada, um comando fixo que só lê o uso de CPU,
  memória, swap e disco, a carga, o tempo ligado e o número de processos e de CPUs, para
  mostrá-los na tela. Isso pode ser desligado em cada conexão, e nada disso é gravado
  (seções 2 e 4).
- O app só se conecta aos **servidores que você cadastra**. Ele não acessa a internet por
  conta própria: não procura atualizações e não consulta o GitHub nem nenhum outro servidor
  do desenvolvedor.
- O desenvolvedor não vende nem compartilha dados com anunciantes ou outras empresas.

## 1. Dados guardados no seu computador

### Cofre (arquivo `.sagu`)

Para cada conexão, o cofre guarda nome, endereço, porta, usuário e senha, ou chave privada
(com a passphrase, se houver). Ele fica num arquivo `.sagu`, no local que você escolher ao
criá-lo.

Depois que você confia num servidor, o cofre guarda também a **chave pública** que ele
apresentou (a *host key*), para conferir as próximas conexões (seção 2). Ela é apagada
quando você usa **Esquecer chave** no editor da conexão, troca o endereço ou a porta da
conexão, ou a exclui.

A partir da versão 1.1.0, o cofre guarda também o **sistema operacional** detectado em cada
servidor (seção 2), em três campos: o identificador, o nome e a versão informados pelo
próprio servidor (por exemplo, `almalinux`, "AlmaLinux" e "8.10"; a versão pode faltar).
Eles servem para mostrar o ícone da distribuição no cartão da conexão e o nome e a versão
do sistema na dica do cartão. Esses dados são substituídos quando o servidor informa outro
sistema ou outra versão, e uma detecção sem resultado não apaga o que já estava guardado.
Eles são apagados quando você troca o endereço ou a porta da conexão, desliga a detecção
no editor dela ou a exclui, e também quando você aceita uma chave nova do servidor (o
sistema é detectado de novo). O **Esquecer chave** não os apaga. O cofre guarda ainda,
para cada conexão, se a detecção está ligada.

- Todo o conteúdo é criptografado com **AES-256-GCM**. A chave de 256 bits é derivada da
  sua senha mestra com **Argon2id** e de um *salt* aleatório. Um *nonce* aleatório novo é
  gerado a cada gravação. A partir da versão 1.2.0, o *salt* é mantido nas gravações
  seguintes; até a 1.1.0, um *salt* novo era gerado a cada gravação.
- A senha mestra não é gravada em lugar nenhum. Se você esquecê-la, não há como recuperar
  o cofre.
- Ao salvar alterações, o app grava primeiro um arquivo temporário `.sagu.tmp` (também
  criptografado) ao lado do cofre e depois o renomeia por cima do original. Se a gravação
  falhar no meio, esse temporário pode sobrar na pasta.
- Quando você importa uma chave privada, o app lê o arquivo escolhido por você no diálogo
  e passa a guardar o conteúdo dele dentro do cofre. O app não lê a pasta `.ssh` nem usa
  um agente SSH por conta própria.
- As versões antigas 0.1.x do SaguTerm não conhecem as chaves dos servidores. Se você abrir
  o cofre numa delas e salvar alguma alteração (editar ou excluir uma conexão), as chaves
  guardadas são apagadas, e esta versão volta a perguntar por elas como se cada servidor
  fosse novo. Por isso, não use o cofre nas versões 0.1.x.
- As versões 1.0.0 e 1.0.1 não conhecem o sistema detectado. Se você salvar alguma
  alteração no cofre numa delas, essa informação é descartada e volta a ser detectada na
  próxima conexão feita a partir da versão 1.1.0, inclusive nas conexões em que você tinha
  desligado a detecção (ela volta a ficar ligada). As chaves dos servidores não são
  afetadas.

### Na memória, enquanto o cofre está aberto

Para poder salvar as suas alterações, o app mantém na memória, enquanto o cofre está
aberto, o conteúdo do cofre e a chave derivada da senha mestra. A partir da versão 1.2.0,
a senha mestra não fica guardada: o app a descarta logo depois de abrir o cofre (até a
1.1.0, ela ficava na memória até o cofre ser bloqueado). Ao bloquear o cofre, o app
apaga a chave da memória ativamente e descarta o conteúdo do cofre, mas não sobrescreve a
memória que o conteúdo ocupava.

### Abrir sem senha neste computador

A partir da versão 1.2.0, a tela de conexões tem a opção **Abrir sem senha neste
computador**, que vem desligada. Ao ligá-la, o app guarda no arquivo
`%APPDATA%\SaguTerm\data\remembered.json` a chave de 256 bits do cofre aberto (nunca a
senha mestra), protegida pela sua conta do Windows com a DPAPI, a proteção de dados do
próprio Windows. Junto vai o *salt* do cofre, que identifica a qual arquivo a chave
pertence. Na versão da Microsoft Store, esse arquivo fica na pasta privada do app, como o
`app.ron` (seção 4).

- Ao iniciar o app, se o último cofre aberto tem a chave guardada, o app o abre direto,
  sem pedir a senha.
- Só a mesma conta do Windows, no mesmo computador, consegue usar a chave guardada. O
  arquivo `.sagu` copiado para outro computador, ou aberto por outra conta do Windows,
  continua pedindo a senha.
- Qualquer programa que rode com a sua conta do Windows também consegue pedir ao Windows a
  chave guardada, e quem usar o computador com a sua sessão do Windows aberta consegue abrir
  o cofre. A proteção do cofre passa a ser a da sua conta do Windows: use senha ou PIN no
  Windows e bloqueie a tela (`Win+L`) ao se afastar.
- Bloquear o cofre não apaga a chave guardada, mas faz a próxima abertura do app pedir a
  senha. Digitá-la religa a abertura automática.
- Desmarcar a opção apaga a chave guardada daquele cofre. Se o Windows não devolver a chave
  (por exemplo, depois de a senha da conta do Windows ser redefinida), o app a apaga e volta
  a pedir a senha.
- Se o cofre for salvo na versão 1.1.0 ou anterior, a chave guardada deixa de servir, porque
  essas versões trocam o *salt* a cada gravação, e o app volta a pedir a senha.

### Histórico do terminal

O que passa pela tela de cada terminal (até as últimas 5.000 linhas por painel) fica só na
memória, para você poder rolar para cima; nunca é gravado em disco. O comando `clear` (ou
`cls`, no Prompt de Comando) tira essas linhas da rolagem, mas elas continuam na memória
até serem substituídas por saída nova, até o comando `reset` ou até o painel ser fechado.
Ao descartá-las, o app não sobrescreve a memória que elas ocupavam.

### Preferências

O arquivo `%APPDATA%\SaguTerm\data\app.ron` guarda o caminho do último cofre aberto, a
posição e o tamanho da janela e o estado da interface (por exemplo, zoom e posições de
rolagem). Ele não contém senhas, conteúdo do cofre nem o texto digitado nos campos, e é
salvo de tempos em tempos e ao fechar o app.

Na versão da Microsoft Store, o Windows normalmente guarda esse arquivo numa pasta privada
do app e o remove quando o app é desinstalado. Se o arquivo já existia porque você usou
antes um executável avulso do SaguTerm, a versão da Store pode continuar usando esse mesmo
arquivo, e ele então não é removido na desinstalação.

### Área de transferência

- Ao terminar de selecionar texto com o mouse no terminal ou no visualizador de arquivos do
  navegador SFTP, o texto selecionado é **copiado automaticamente** para a área de
  transferência do Windows. Cuidado ao selecionar senhas ou outros dados sensíveis.
- O app só lê a área de transferência quando você manda colar (botão direito sobre o
  terminal, ou `Ctrl+V`). O texto colado vai para o terminal em foco, seja um servidor
  remoto ou um shell local.
- Se o histórico ou a sincronização da área de transferência do Windows estiverem
  ativados, o texto copiado também segue as regras do Windows.

### Arquivos do seu computador

- O app só lê os arquivos que você indica: o cofre, a chave privada que você importa e os
  arquivos que você arrasta do Explorador de Arquivos para um painel SFTP ou para um
  terminal SSH. Os arquivos arrastados são enviados ao servidor daquele painel (veja a
  seção 2). Pastas arrastadas não são enviadas.
- **Downloads:** quando você baixa arquivos pelo navegador SFTP, o app grava só na pasta
  que você escolhe no diálogo do Windows (o diálogo sugere a sua pasta Downloads). Antes de
  começar, ele confere quais dos itens escolhidos já existem nessa pasta, para perguntar
  antes de substituir. Cada arquivo é gravado primeiro num temporário
  `.<nome>.XXXXXXXX.sagu-part` na mesma pasta e só recebe o nome final quando termina. Se
  o download falhar ou for cancelado, o app apaga o temporário; se o app for fechado no
  meio do download, ou o computador desligar, ele pode sobrar. A última pasta escolhida
  fica lembrada só enquanto o app está aberto (o próprio diálogo do Windows também pode
  lembrá-la, conforme as regras do Windows). O botão **Abrir pasta** abre essa pasta no
  Explorador de Arquivos.
- Além disso, o app apenas verifica se o WSL está instalado (se o `wsl.exe` do Windows
  existe) e se a sua pasta Downloads existe, para sugeri-la como destino. Ele não varre
  pastas nem lê outros arquivos por conta própria.

### Terminal local

O Prompt de Comando (`cmd.exe`) e o WSL rodam no seu computador. O que você digita neles
vai só para esses programas. O que os comandos executados fazem, como acessar a internet,
depende desses programas, não do SaguTerm.

## 2. Conexões de rede

### Servidores que você cadastra

Quando você abre uma sessão SSH ou SFTP, o SaguTerm se conecta diretamente, sem
intermediários, ao endereço e à porta cadastrados. O nome do servidor é resolvido pelo DNS
configurado no Windows. Pela conexão SSH, que é criptografada, vão:

- o usuário e a senha, ou uma assinatura feita com a sua chave privada (a chave em si não
  é enviada);
- tudo o que você digita ou cola no terminal;
- os arquivos que você envia e as alterações que você pede no navegador SFTP (renomear,
  alterar permissões ou dono, excluir);
- a identificação padrão do cliente SSH (nome e versão da biblioteca SSH usada pelo app).

Esses dados são recebidos e tratados pelo servidor conforme as regras de quem o administra,
e o SaguTerm não controla isso.

Do servidor, o app lê apenas o necessário para as funções que você usa:

- a chave pública que o servidor apresenta em toda conexão, para compará-la com a guardada
  no cofre;
- a saída do terminal;
- no navegador SFTP, a lista de pastas e arquivos (nome, data de modificação, tamanho,
  dono, grupo e permissões; num link simbólico, também o destino dele e esses mesmos dados
  do destino, para mostrar se ele aponta para uma pasta ou para um arquivo) e os arquivos
  `/etc/passwd` e `/etc/group`, para mostrar nomes de usuários e grupos em vez de números.
  Tudo isso fica só na memória;
- os arquivos e pastas que você manda baixar (conteúdo, tamanho e data de modificação),
  gravados na pasta que você escolheu (seção 1);
- a partir da versão 1.1.0, **os arquivos e pastas que você copia ou move** no navegador
  SFTP (`Ctrl+C`/`Ctrl+X` e `Ctrl+V`), no mesmo servidor. Mover é um pedido de renomear ao
  servidor. Copiar lê cada arquivo e grava a cópia na pasta de destino do mesmo servidor,
  com as permissões e as datas do original (e, conectado como `root`, o dono e o grupo); os
  dados passam só pela memória do app, em blocos, e não são gravados no seu computador.
  Mover para outro disco do servidor só copia e apaga os originais depois de você
  confirmar;
- a partir da versão 1.4.0, **os arquivos e pastas que você arrasta de um painel SFTP para
  outro**, inclusive de um servidor para outro dos que você cadastrou. O app lê cada
  arquivo pela conexão do painel de origem e grava a cópia, pela conexão do painel de
  destino, na pasta aberta nele, com as permissões e as datas do original (o dono e o grupo
  nunca passam de um servidor para outro); os dados passam só pela memória do app, em
  blocos, e não são gravados no seu computador. Arrastar só copia: nada é movido nem
  apagado na origem;
- a partir da versão 1.1.0, **o arquivo que você abre no visualizador** do navegador SFTP
  (Enter ou duplo clique sobre ele), só para mostrá-lo na tela: o tipo, o tamanho e a data
  de modificação dele (num link simbólico, também o destino; num arquivo de tamanho 0,
  também o caminho real, sem links) e o começo do conteúdo, até cerca de 4 MiB. Antes de
  ler o conteúdo, o app confere o tipo e não abre pastas, arquivos especiais (fifo, socket
  ou dispositivo) nem interfaces do kernel cuja leitura bloqueia ou consome os dados (como
  o `/proc/kmsg`); se o começo do arquivo parecer binário, ele para de ler e não mostra
  nada. O conteúdo fica só na memória do app e nunca é gravado em disco; ele é descartado
  quando você fecha o visualizador ou o painel;
- a partir da versão 1.1.0, **o sistema operacional do servidor**, para mostrar o ícone da
  distribuição no cartão da conexão. Cerca de 2 segundos depois de a sessão SSH ou SFTP
  abrir, o app executa nesse servidor, com o seu usuário e num canal separado da mesma
  conexão, sempre este mesmo comando:

  ```text
  echo SAGUOS.uname; uname -s -r; echo SAGUOS.etc; cat /etc/os-release; echo SAGUOS.lib; cat /usr/lib/os-release; echo SAGUOS.rh; cat /etc/redhat-release; echo SAGUOS.sw; sw_vers; echo SAGUOS.end
  ```

  Esse comando só lê informações do sistema: o nome e a versão do kernel (`uname -s -r`),
  os arquivos `/etc/os-release`, `/usr/lib/os-release` e `/etc/redhat-release` e, no
  macOS, a versão do sistema (`sw_vers`). Os `echo SAGUOS...` só marcam onde começa cada
  parte da resposta. O que não existe no servidor (como o `sw_vers` fora do macOS) gera apenas uma
  mensagem de erro, que o app ignora. A resposta não aparece no terminal; o app lê no
  máximo 64 KiB dela, guarda no cofre só o identificador, o nome e a versão do sistema
  (seção 1), com tamanho limitado e sem caracteres de controle, e descarta o resto. Em
  geral, isso acontece uma vez por conexão cadastrada a cada abertura do cofre, e de novo
  se você trocar o endereço ou a porta dela ou aceitar uma chave nova do servidor (duas
  sessões abertas ao mesmo tempo no mesmo servidor podem repeti-lo). Como qualquer comando
  executado por SSH, ele pode ficar registrado nos logs do servidor, e o servidor pode
  carregar antes os arquivos de inicialização do seu shell (como o `.bashrc`). **Se o
  servidor força um comando** (`ForceCommand` no `sshd_config` ou `command="..."` no
  `authorized_keys`), é esse comando do servidor que roda no lugar do comando acima, sem
  terminal e com a entrada fechada, uma vez a mais a cada detecção; os scripts `~/.ssh/rc`
  e `/etc/ssh/sshrc`, se existirem, também rodam de novo, como em qualquer sessão SSH. Para
  evitar isso, desligue a detecção dessa conexão (seção 4). Se o servidor recusar o
  comando, levar mais de 10 segundos para abrir o canal ou para responder, ou mandar mais
  de 64 KiB, o app desiste sem avisar e o cartão continua com o ícone que já tinha;
- a identificação que todo servidor SSH envia no início da conexão (por exemplo,
  `SSH-2.0-OpenSSH_for_Windows_9.5`). O app a usa só na memória, para reconhecer servidores
  Windows e alguns equipamentos de rede, nos quais o comando acima não é executado;
- a partir da versão 1.3.0, **a saúde do servidor**, só enquanto a tela **Monitoramento**
  estiver aberta. O app conecta a cada conexão cadastrada (menos as que têm **Detectar o
  sistema do servidor** desmarcado) e executa nela, com o seu usuário e num canal sem
  terminal, sempre este mesmo comando: ao abrir a tela, uma vez por minuto e na hora quando
  você clica em **Atualizar agora** ou muda a lista de conexões monitoradas (cadastra ou
  exclui uma conexão, troca o endereço, a porta, o usuário, a senha ou a chave dela, liga
  ou desliga a detecção, aceita ou esquece a chave de um servidor):

  ```text
  echo SAGUMON.load; cat /proc/loadavg; echo SAGUMON.cpus; getconf _NPROCESSORS_ONLN; nproc; echo SAGUMON.mem; cat /proc/meminfo; echo SAGUMON.up; cat /proc/uptime; echo SAGUMON.stat; head -n 1 /proc/stat; sleep 1; head -n 1 /proc/stat; echo SAGUMON.df; env LC_ALL=C timeout -s KILL 5 df -P -k; env LC_ALL=C timeout -t 5 -s KILL df -P -k; echo SAGUMON.end
  ```

  Esse comando só lê a carga do sistema e o número de processos (`/proc/loadavg`), o
  número de CPUs (`getconf` ou `nproc`), o uso de memória e de swap (`/proc/meminfo`), o
  tempo ligado (`/proc/uptime`), os contadores de uso de CPU (a primeira linha do
  `/proc/stat`, duas vezes, com 1 segundo entre elas) e o espaço dos sistemas de arquivos
  montados (`df -P -k`, em inglês por causa do `LC_ALL=C` e encerrado pelo `timeout` se
  passar de 5 segundos, como num compartilhamento de rede fora do ar; ele aparece duas
  vezes, uma em cada forma de escrever o `timeout`, e a que o servidor não entende falha
  na hora, sem rodar o `df`). Os `echo SAGUMON...` só marcam onde começa cada parte da
  resposta. A resposta não aparece em nenhum terminal; o app lê no máximo 1 MiB dela,
  mostra os números na tela e guarda só na memória a última leitura (com os contadores de
  CPU, para calcular o uso médio até a coleta seguinte, e a última lista de discos, para
  quando o `df` não responder) e o histórico de CPU e memória da última hora, descartados
  quando você fecha a tela. Nada vai para o cofre nem para o disco. A conexão fica aberta
  enquanto a tela estiver aberta e cai quando você a fecha: em geral, um login por
  abertura da tela, não um por minuto. Se a conexão cair, o app conecta de novo na coleta
  seguinte. Se o servidor recusar as credenciais, o app não tenta de novo sozinho, para
  não encher os logs do servidor de tentativas de login falhas nem acionar bloqueios como
  o fail2ban: só quando você clica em **Atualizar agora** ou edita a conexão. Se a chave
  do servidor ainda não foi confirmada, ou se mudou, o app desiste antes de enviar o
  usuário, a senha ou a assinatura da chave, e o cartão pede que você conecte pelo
  terminal. Em servidores Windows e equipamentos de rede (reconhecidos pela identificação
  do item anterior), o comando não é executado. Como qualquer comando executado por SSH,
  ele pode ficar registrado nos logs do servidor, e o servidor pode carregar antes os
  arquivos de inicialização do seu shell. **Se o servidor força um comando**
  (`ForceCommand` no `sshd_config` ou `command="..."` no `authorized_keys`), é esse
  comando do servidor que roda no lugar do comando acima, a cada coleta, e os scripts
  `~/.ssh/rc` e `/etc/ssh/sshrc`, se existirem, também rodam a cada coleta; nesses
  servidores, desmarque **Detectar o sistema do servidor** (seção 4);
- ao soltar arquivos num terminal SSH, o app executa nesse servidor, com o seu usuário, um
  pequeno script embutido no app. O script descobre a pasta atual do shell lendo
  informações dos processos em `/proc` (e do tmux, se houver). O resultado fica só na
  memória e serve para escolher a pasta de destino.

Ao soltar arquivos num terminal SSH, um arquivo que já existe no servidor só é substituído
com a sua autorização: o app grava primeiro um temporário `.<nome>.sagu-XXXXXXXX.part` na
mesma pasta e depois o renomeia. Ao soltar arquivos num painel SFTP, um arquivo de mesmo
nome na pasta aberta é substituído direto, sem pergunta.

**Chave do servidor:** na primeira conexão com um servidor, o app mostra o tipo e a
impressão digital SHA256 da chave dele e só continua se você confiar nela. Se depois o
servidor apresentar outra chave, o app mostra um alerta e só continua se você aceitar a
chave nova. Essa conferência acontece antes de o app enviar o usuário, a senha ou a
assinatura da chave privada. Se você cancelar, a conexão é encerrada e nada é gravado. O
app não lê nem altera o arquivo `known_hosts` do OpenSSH.

### Atualizações

As atualizações são entregues pela própria Microsoft Store. O SaguTerm não tem atualizador
próprio: ele não consulta o GitHub nem nenhum outro servidor do desenvolvedor e não baixa
nada da internet por conta própria. As únicas conexões de rede que o app abre são as que
você pede, para os servidores que você cadastra.

## 3. Com quem os dados são compartilhados

- **Com os servidores que você configura**, e só com eles, quando você se conecta (seção 2).
- **Microsoft:** se você instalou pela Microsoft Store, o download, a instalação e as
  atualizações são feitos pela Microsoft, conforme a Declaração de Privacidade da
  Microsoft. A Microsoft pode disponibilizar ao desenvolvedor, no Partner Center,
  relatórios de aquisições e de uso e informações sobre falhas do app (como travamentos),
  que ela coleta conforme as configurações de diagnóstico do seu Windows. O SaguTerm não
  acrescenta nada a esses relatórios.
- **GitHub:** o código-fonte e as issues do SaguTerm ficam no GitHub. Se você visitar o
  repositório ou abrir uma issue, vale a Declaração de Privacidade do GitHub. O app em si
  não se comunica com o GitHub.
- O desenvolvedor não vende, não aluga e não compartilha dados de usuários com ninguém,
  porque não os recebe.

## 4. Seus controles

- **Excluir uma conexão:** clique com o botão direito no cartão da conexão e escolha
  **Excluir**. O cofre é regravado sem ela.
- **Esquecer a chave de um servidor:** clique com o botão direito no cartão da conexão,
  escolha **Editar**, clique em **Esquecer chave** e em **Salvar**. A chave será pedida de
  novo na próxima conexão.
- **Sistema detectado de um servidor:** sai do cofre junto com a conexão, quando você troca
  o endereço ou a porta dela no editor ou quando aceita uma chave nova do servidor (seção 1).
  Para não executar o comando de detecção num servidor, clique com o botão direito no cartão
  da conexão, escolha **Editar**, desmarque **Detectar o sistema do servidor** e clique em
  **Salvar**. O sistema já detectado dessa conexão é apagado, e o cartão volta ao ícone de
  servidor.
- **Monitoramento:** o comando só roda com a tela **Monitoramento** aberta; feche o painel
  para parar as coletas e encerrar as conexões dela. Para deixar um servidor de fora,
  desmarque **Detectar o sistema do servidor** no editor da conexão.
- **Bloquear o cofre:** `Ctrl+L` na tela de conexões ou o botão **Bloquear cofre**. Com
  **Abrir sem senha neste computador** ligado, a próxima abertura do app volta a pedir a
  senha.
- **Abrir sem senha neste computador:** desmarque a opção na tela de conexões para apagar
  a chave guardada daquele cofre. Para apagar as de todos os cofres, com o app fechado,
  apague o arquivo `remembered.json`, que fica na mesma pasta do `app.ron`.
- **Apagar o cofre:** apague o arquivo `.sagu` (e um `.sagu.tmp` que tenha sobrado ao lado
  dele). Isso apaga todas as conexões guardadas.
- **Apagar as preferências:** com o app fechado, apague `%APPDATA%\SaguTerm\data\app.ron`.
  Na versão da Microsoft Store, se o arquivo foi criado por ela, ele fica na pasta privada
  do app (seção 1):
  `%LOCALAPPDATA%\Packages\<pasta do SaguTerm>\LocalCache\Roaming\SaguTerm\data\app.ron`,
  em que a pasta do SaguTerm é a que tem "SaguTerm" no nome. Outra opção é Configurações >
  Aplicativos > Aplicativos instalados > SaguTerm > Opções avançadas > **Redefinir**, que
  apaga todos os dados privados do app e pode apagar também um cofre salvo dentro de
  `AppData`.
- **Área de transferência:** limpe-a pelo Windows, em Configurações > Sistema > Área de
  transferência.
- **Desinstalar:** a versão da Store é removida em Configurações > Aplicativos >
  Aplicativos instalados, junto com os dados privados que o Windows guardou para ela. Um
  cofre salvo numa pasta sua, como Documentos, **não** é apagado na desinstalação; apague-o
  você mesmo, se quiser. Por outro lado, na versão da Store, um cofre criado dentro de
  `AppData` pode ser apagado junto com o app. Por isso, prefira guardar o cofre numa pasta
  sua. Um executável compilado por você é removido apagando o `SaguTerm.exe` e a pasta
  `%APPDATA%\SaguTerm`.
- **Arquivos baixados** ficam na pasta que você escolheu. Apague-os por lá, se quiser. A
  desinstalação não remove o que foi baixado para uma pasta sua, como Downloads ou
  Documentos.
- **Dados enviados aos seus servidores** ficam nesses servidores. Para removê-los, fale com
  quem os administra.

## 5. Crianças

O SaguTerm é uma ferramenta técnica para administração de servidores e não é direcionado a
crianças. O app não coleta dados de ninguém, incluindo crianças.

## 6. Seus direitos (LGPD)

O desenvolvedor não recebe dados pessoais pelo SaguTerm e, portanto, não mantém dados seus
que possam ser acessados, corrigidos ou apagados a seu pedido, nos termos da Lei Geral de
Proteção de Dados (Lei nº 13.709/2018). Os dados guardados no app ficam sob o seu controle,
no seu computador. Para dados tratados pela Microsoft, pelo GitHub ou pelos administradores
dos servidores a que você se conecta, procure essas partes.

## 7. Alterações desta política

Se o app passar a tratar dados de outra forma, esta política será atualizada, com nova data
de vigência, antes ou junto com a versão que trouxer a mudança. O histórico de alterações
fica no histórico deste arquivo no repositório.

## 8. Contato

Abra uma issue em <https://github.com/alowelter/sagu-term/issues>. As issues são
públicas: não inclua senhas, chaves, endereços de servidores nem outros dados pessoais.

---

<a id="english"></a>

# SaguTerm Privacy Policy

**Effective date:** October 10, 2026

This policy applies to SaguTerm distributed through the **Microsoft Store** and also to an
executable you build yourself from the source code, which is public on GitHub. SaguTerm is
an open source (MIT license) SSH/SFTP client with a local terminal for Windows, maintained
by Marcelo Welter ("the developer"). If the Portuguese and English versions differ, the
Portuguese version prevails.

## Summary

- **SaguTerm sends no data to the developer.** There is no account, sign-up, ads,
  telemetry, usage statistics or crash reporting of its own, and the app writes no logs to
  disk.
- Your connections, passwords and keys, the public keys of the servers you accepted, and
  the operating system detected on each server are kept in an **encrypted vault**, in a
  file stored wherever you choose. If you turn on **Abrir sem senha neste computador** (Open
  without password on this computer), the vault's key is kept on your computer, protected
  by your Windows account (section 1).
- Files you download from your servers go only to the folder you choose.
- The file viewer in the SFTP browser reads from the server only the file you open, to show
  it on screen. Its content stays in memory and is not written to disk.
- Copying and moving files in the SFTP browser (`Ctrl+C`/`Ctrl+X` and `Ctrl+V`) happens on
  the server itself, and dragging files from one SFTP pane to another copies them to that
  pane's server, which may be another of the servers you registered: the data of a copy
  passes through the app's memory without being written to your computer.
- When connecting to a server, the app may run on it, as your user, a fixed command that
  only reads the operating system's identification, to show the system's icon on the
  connection card. This can be turned off for each connection (sections 2 and 4).
- While the **Monitoramento** (Monitoring) screen is open, the app runs every minute, on
  each registered server with system detection on, a fixed command that only reads CPU,
  memory, swap and disk usage, the load, the uptime and the number of processes and CPUs,
  to show them on screen. This can be turned off for each connection, and none of it is
  saved (sections 2 and 4).
- The app only connects to the **servers you register**. It does not access the internet
  on its own: it does not check for updates and does not contact GitHub or any other server
  of the developer.
- The developer does not sell or share any data with advertisers or other companies.

## 1. Data stored on your computer

### Vault (`.sagu` file)

For each connection, the vault stores the name, address, port, user name and password, or
private key (with its passphrase, if any). It is a `.sagu` file, saved at the location you
choose when you create it.

Once you trust a server, the vault also stores the **public key** it presented (its *host
key*), to check later connections (section 2). It is deleted when you use **Esquecer
chave** (Forget key) in the connection editor, change the connection's address or port, or
delete the connection.

Starting with version 1.1.0, the vault also stores the **operating system** detected on
each server (section 2), in three fields: the identifier, name and version reported by the
server itself (for example, `almalinux`, "AlmaLinux" and "8.10"; the version may be
missing). They are used to show the distribution's icon on the connection card and the
system's name and version in the card's tooltip. This data is replaced when the server
reports another system or version, and a detection with no result does not erase what was
already stored. It is deleted when you change the connection's address or port, turn
detection off in the connection editor, or delete the connection, and also when you accept
a new key from the server (the system is detected again). **Esquecer chave** (Forget key)
does not delete it. The vault also stores, for each connection, whether detection is on.

- All of its content is encrypted with **AES-256-GCM**. The 256-bit key is derived from
  your master password and a random salt with **Argon2id**. A new random nonce is generated
  on every save. Starting with version 1.2.0, the salt is kept on later saves; up to
  1.1.0, a new salt was generated on every save.
- The master password is never stored anywhere. If you forget it, the vault cannot be
  recovered.
- When saving changes, the app first writes a temporary `.sagu.tmp` file (also encrypted)
  next to the vault and then renames it over the original. If the write fails midway, this
  temporary file may be left in the folder.
- When you import a private key, the app reads the file you picked in the dialog and from
  then on keeps its content inside the vault. The app does not read your `.ssh` folder or
  use an SSH agent on its own.
- Old SaguTerm versions 0.1.x do not know about server keys. If you open the vault in one
  of them and save any change (editing or deleting a connection), the stored keys are
  deleted, and this version asks for them again as if each server were new. For this
  reason, do not use the vault with versions 0.1.x.
- Versions 1.0.0 and 1.0.1 do not know about the detected operating system. If you save
  any change to the vault in one of them, this information is discarded and detected again
  on the next connection made with version 1.1.0 or later, including on connections where
  you had turned detection off (it is turned back on). Server keys are not affected.

### In memory, while the vault is open

To be able to save your changes, the app keeps the vault content and the key derived from
the master password in memory while the vault is open. Starting with version 1.2.0, the
master password itself is not kept: the app discards it right after opening the vault (up to
1.1.0, it stayed in memory until the vault was locked). When you lock the vault, the app
actively wipes the key from memory and discards the vault content, but does not overwrite
the memory the content used.

### Open without password on this computer

Starting with version 1.2.0, the connections screen has the option **Abrir sem senha neste
computador** (Open without password on this computer), which is off by default. When you
turn it on, the app stores in the file `%APPDATA%\SaguTerm\data\remembered.json` the
256-bit key of the open vault (never the master password), protected by your Windows
account with DPAPI, Windows' own data protection. Next to it goes the vault's salt, which
identifies which file the key belongs to. In the Microsoft Store version, this file is in
the app's private folder, like `app.ron` (section 4).

- When the app starts, if the last opened vault has its key stored, the app opens it
  directly, without asking for the password.
- Only the same Windows account, on the same computer, can use the stored key. The `.sagu`
  file copied to another computer, or opened by another Windows account, still asks for the
  password.
- Any program running under your Windows account can also ask Windows for the stored key,
  and anyone using the computer with your Windows session unlocked can open the vault. The
  vault's protection becomes that of your Windows account: use a password or PIN in Windows
  and lock the screen (`Win+L`) when you step away.
- Locking the vault does not delete the stored key, but makes the next start of the app ask
  for the password. Typing it turns automatic opening back on.
- Unchecking the option deletes that vault's stored key. If Windows does not return the key
  (for example, after the Windows account password is reset), the app deletes it and asks
  for the password again.
- If the vault is saved with version 1.1.0 or earlier, the stored key no longer works,
  because those versions change the salt on every save, and the app asks for the password
  again.

### Terminal history

What scrolls through each terminal (up to the last 5,000 lines per pane) is kept only in
memory, so you can scroll back; it is never written to disk. The `clear` command (or `cls`,
in Command Prompt) removes those lines from scrollback, but they stay in memory until newer
output replaces them, until you run `reset`, or until the pane is closed. When discarding
them, the app does not overwrite the memory they used.

### Settings

The file `%APPDATA%\SaguTerm\data\app.ron` stores the path of the last opened vault, the
window position and size, and the user interface state (for example, zoom and scroll
positions). It contains no passwords, no vault content and no text typed into fields. It is
saved periodically and when the app closes.

In the Microsoft Store version, Windows normally keeps this file in a private folder for
the app and removes it when the app is uninstalled. If the file already existed because you
used a standalone SaguTerm executable before, the Store version may keep using that same
file, which is then not removed on uninstall.

### Clipboard

- When you finish selecting text with the mouse in the terminal or in the SFTP browser's
  file viewer, the selected text is **automatically copied** to the Windows clipboard. Be
  careful when selecting passwords or other sensitive data.
- The app only reads the clipboard when you paste (right-click on the terminal, or
  `Ctrl+V`). Pasted text goes to the focused terminal, either a remote server or a local
  shell.
- If Windows clipboard history or clipboard sync is turned on, copied text is also subject
  to the Windows rules.

### Files on your computer

- The app only reads the files you point it to: the vault, the private key you import, and
  the files you drag from File Explorer onto an SFTP pane or an SSH terminal. Dragged files
  are uploaded to that pane's server (see section 2). Dragged folders are not uploaded.
- **Downloads:** when you download files in the SFTP browser, the app writes only to the
  folder you pick in the Windows dialog (the dialog suggests your Downloads folder). Before
  starting, it checks which of the chosen items already exist in that folder, so it can ask
  before replacing anything. Each file is first written to a temporary
  `.<name>.XXXXXXXX.sagu-part` file in the same folder and only gets its final name when it
  is complete. If the download fails or is cancelled, the app deletes the temporary file; if
  the app is closed in the middle of a download, or the computer shuts down, it may be left
  behind. The last folder you picked is remembered only while the app is open (the Windows
  dialog itself may also remember it, according to the Windows rules). The **Abrir pasta**
  (Open folder) button opens that folder in File Explorer.
- Apart from that, the app only checks whether WSL is installed (whether the Windows
  `wsl.exe` exists) and whether your Downloads folder exists, to suggest it as the
  destination. It does not scan folders or read other files on its own.

### Local terminal

Command Prompt (`cmd.exe`) and WSL run on your computer. What you type in them goes only to
those programs. What the commands you run do, such as accessing the internet, depends on
those programs, not on SaguTerm.

## 2. Network connections

### Servers you register

When you open an SSH or SFTP session, SaguTerm connects directly, with no intermediaries,
to the registered address and port. The server name is resolved by the DNS configured in
Windows. Over the SSH connection, which is encrypted, the app sends:

- the user name and password, or a signature made with your private key (the key itself is
  not sent);
- everything you type or paste into the terminal;
- the files you upload and the changes you request in the SFTP browser (rename, change
  permissions or owner, delete);
- the standard SSH client identification (name and version of the SSH library used by the
  app).

This data is received and handled by the server according to the rules of whoever
administers it, which SaguTerm does not control.

From the server, the app reads only what the features you use need:

- the public key the server presents on every connection, to compare it with the one stored
  in the vault;
- the terminal output;
- in the SFTP browser, the list of folders and files (name, modification date, size, owner,
  group and permissions; for a symbolic link, also its target and the same data about the
  target, to show whether it points to a folder or to a file) and the `/etc/passwd` and
  `/etc/group` files, to show user and group names instead of numbers. All of this stays in
  memory only;
- the files and folders you choose to download (content, size and modification date),
  written to the folder you picked (section 1);
- starting with version 1.1.0, **the files and folders you copy or move** in the SFTP
  browser (`Ctrl+C`/`Ctrl+X` and `Ctrl+V`), on the same server. Moving is a rename request
  to the server. Copying reads each file and writes the copy to the destination folder on
  the same server, with the original's permissions and dates (and, when connected as
  `root`, its owner and group); the data only passes through the app's memory, in blocks,
  and is not written to your computer. Moving to another disk of the server only copies and
  deletes the originals after you confirm;
- starting with version 1.4.0, **the files and folders you drag from one SFTP pane to
  another**, including from one server to another of the servers you registered. The app
  reads each file over the source pane's connection and writes the copy, over the
  destination pane's connection, to the folder open there, with the original's permissions
  and dates (the owner and group never carry over from one server to another); the data
  only passes through the app's memory, in blocks, and is not written to your computer.
  Dragging only copies: nothing is moved or deleted at the source;
- starting with version 1.1.0, **the file you open in the viewer** of the SFTP browser
  (Enter or double-click on it), only to show it on screen: its type, size and modification
  date (for a symbolic link, also its target; for a zero-size file, also its real path,
  without links) and the beginning of its content, up to about 4 MiB. Before reading the
  content, the app checks the type and does not open folders, special files (FIFO, socket
  or device) or kernel interfaces whose reading blocks or consumes the data (such as
  `/proc/kmsg`); if the beginning of the file looks binary, it stops reading and shows
  nothing. The content stays only in the app's memory and is never written to disk; it is
  discarded when you close the viewer or the pane;
- starting with version 1.1.0, **the server's operating system**, to show the
  distribution's icon on the connection card. About 2 seconds after the SSH or SFTP session
  opens, the app runs on that server, as your user and on a separate channel of the same
  connection, always this same command:

  ```text
  echo SAGUOS.uname; uname -s -r; echo SAGUOS.etc; cat /etc/os-release; echo SAGUOS.lib; cat /usr/lib/os-release; echo SAGUOS.rh; cat /etc/redhat-release; echo SAGUOS.sw; sw_vers; echo SAGUOS.end
  ```

  This command only reads system information: the kernel name and version (`uname -s -r`),
  the files `/etc/os-release`, `/usr/lib/os-release` and `/etc/redhat-release` and, on
  macOS, the system version (`sw_vers`). The `echo SAGUOS...` parts only mark where each
  part of the answer begins. Whatever does not exist on the server (such as `sw_vers` outside macOS)
  only produces an error message, which the app ignores. The answer is not shown in the
  terminal; the app reads at most 64 KiB of it, keeps only the system's identifier, name
  and version in the vault (section 1), with limited length and no control characters, and
  discards the rest. This usually happens once per registered connection each time the
  vault is opened, and again if you change the connection's address or port or accept a
  new key from the server (two sessions opened at the same time on the same server may
  repeat it). Like any command run over SSH, it may be recorded in the server's logs, and
  the server may first load your shell's startup files (such as `.bashrc`). **If the server
  forces a command** (`ForceCommand` in `sshd_config` or `command="..."` in
  `authorized_keys`), it is that server command that runs instead of the command above,
  without a terminal and with its input closed, one extra time on each detection; the
  `~/.ssh/rc` and `/etc/ssh/sshrc` scripts, if present, also run again, as in any SSH
  session. To avoid this, turn detection off for that connection (section 4). If the
  server refuses the command, takes more than 10 seconds to open the channel or to answer,
  or sends more than 64 KiB, the app silently gives up and the card keeps the icon it
  already had;
- the identification every SSH server sends at the start of the connection (for example,
  `SSH-2.0-OpenSSH_for_Windows_9.5`). The app uses it, in memory only, to recognize Windows
  servers and some network devices, on which the command above is not run;
- starting with version 1.3.0, **the server's health**, only while the **Monitoramento**
  (Monitoring) screen is open. The app connects to each registered connection (except those
  with **Detectar o sistema do servidor** unchecked) and runs on it, as your user and on a
  channel without a terminal, always this same command: when the screen opens, once a
  minute, and right away when you click **Atualizar agora** (Refresh now) or change the
  list of monitored connections (add or delete a connection, change its address, port,
  user, password or key, turn detection on or off, accept or forget a server's key):

  ```text
  echo SAGUMON.load; cat /proc/loadavg; echo SAGUMON.cpus; getconf _NPROCESSORS_ONLN; nproc; echo SAGUMON.mem; cat /proc/meminfo; echo SAGUMON.up; cat /proc/uptime; echo SAGUMON.stat; head -n 1 /proc/stat; sleep 1; head -n 1 /proc/stat; echo SAGUMON.df; env LC_ALL=C timeout -s KILL 5 df -P -k; env LC_ALL=C timeout -t 5 -s KILL df -P -k; echo SAGUMON.end
  ```

  This command only reads the system load and the number of processes (`/proc/loadavg`),
  the number of CPUs (`getconf` or `nproc`), memory and swap usage (`/proc/meminfo`), the
  uptime (`/proc/uptime`), the CPU usage counters (the first line of `/proc/stat`, twice,
  1 second apart) and the space of the mounted file systems (`df -P -k`, in English
  because of `LC_ALL=C`, and stopped by `timeout` if it takes more than 5 seconds, as with
  a network share that is down; it appears twice, once in each way of writing `timeout`,
  and the one the server does not understand fails right away without running `df`). The
  `echo SAGUMON...` lines only mark where each part of the answer starts. The answer does
  not appear in any terminal; the app reads at most 1 MiB of it, shows the numbers on
  screen and keeps, in memory only, the latest reading (with the CPU counters, to compute
  the average usage until the next collection, and the last list of disks, for when `df`
  does not answer) and the last hour of CPU and memory history, discarded when you close
  the screen. Nothing goes to the vault or to disk. The connection stays open while the
  screen is open and is closed when you close it: usually one login per opening of the
  screen, not one per minute. If the connection drops, the app connects again at the next
  collection. If the server rejects the credentials, the app does not try again on its
  own, so as not to fill the server's logs with failed login attempts or trigger blocks
  such as fail2ban: only when you click **Atualizar agora** (Refresh now) or edit the
  connection. If the server's key has not been confirmed yet, or has changed, the app gives
  up before sending the user name, the password or the key signature, and the card asks
  you to connect through the terminal. On Windows servers and network devices (recognized
  by the identification in the previous item) the command is not run. Like any command run
  over SSH, it may be recorded in the server's logs, and the server may first load your
  shell's startup files. **If the server forces a command** (`ForceCommand` in
  `sshd_config` or `command="..."` in `authorized_keys`), that server command runs instead
  of the command above, at each collection, and the `~/.ssh/rc` and `/etc/ssh/sshrc`
  scripts, if present, also run at each collection; on such servers, uncheck **Detectar o
  sistema do servidor** (Detect the server's system) (section 4);
- when you drop files onto an SSH terminal, the app runs on that server, as your user, a
  small script embedded in the app. The script finds the shell's current folder by reading
  process information under `/proc` (and from tmux, if present). The result stays in memory
  only and is used to choose the destination folder.

When you drop files onto an SSH terminal, a file that already exists on the server is only
replaced with your permission: the app first writes a temporary `.<name>.sagu-XXXXXXXX.part`
file in the same folder and then renames it. When you drop files onto an SFTP pane, a file
with the same name in the open folder is replaced directly, without asking.

**Server key:** on the first connection to a server, the app shows the type and the SHA256
fingerprint of its key and only continues if you trust it. If the server later presents a
different key, the app shows a warning and only continues if you accept the new key. This
check happens before the app sends the user name, the password or the private key
signature. If you cancel, the connection is closed and nothing is saved. The app does not
read or change OpenSSH's `known_hosts` file.

### Updates

Updates are delivered by the Microsoft Store itself. SaguTerm has no updater of its own: it
does not contact GitHub or any other server of the developer, and it does not download
anything from the internet on its own. The only network connections the app opens are the
ones you ask for, to the servers you register.

## 3. Who data is shared with

- **The servers you configure**, and only them, when you connect (section 2).
- **Microsoft:** if you installed from the Microsoft Store, download, installation and
  updates are handled by Microsoft under the Microsoft Privacy Statement. Microsoft may
  provide the developer, in Partner Center, with acquisition and usage reports and with
  information about app failures (such as crashes), which Microsoft collects according to
  your Windows diagnostic settings. SaguTerm adds nothing to these reports.
- **GitHub:** SaguTerm's source code and issues are hosted on GitHub. If you visit the
  repository or open an issue, the GitHub Privacy Statement applies. The app itself does
  not communicate with GitHub.
- The developer does not sell, rent or share user data with anyone, because the developer
  does not receive it.

## 4. Your controls

- **Delete a connection:** right-click the connection card and choose **Excluir**
  (Delete). The vault is saved again without it.
- **Forget a server's key:** right-click the connection card, choose **Editar** (Edit),
  click **Esquecer chave** (Forget key) and then **Salvar** (Save). The key will be asked
  for again on the next connection.
- **Detected operating system of a server:** it leaves the vault together with the
  connection, when you change the connection's address or port in the editor, or when you
  accept a new key from the server (section 1). To stop running the detection command on a
  server, right-click the connection card, choose **Editar** (Edit), uncheck **Detectar o
  sistema do servidor** (Detect the server's system) and click **Salvar** (Save). The system
  already detected for that connection is deleted, and the card goes back to the server icon.
- **Monitoring:** the command only runs while the **Monitoramento** (Monitoring) screen is
  open; close the pane to stop collecting and close its connections. To leave a server out,
  uncheck **Detectar o sistema do servidor** (Detect the server's system) in the
  connection's editor.
- **Lock the vault:** `Ctrl+L` on the connections screen, or the **Bloquear cofre** (Lock
  vault) button. With **Abrir sem senha neste computador** (Open without password on this
  computer) on, the next start of the app asks for the password again.
- **Open without password on this computer:** uncheck the option on the connections screen
  to delete that vault's stored key. To delete the keys of all vaults, with the app closed,
  delete the `remembered.json` file, in the same folder as `app.ron`.
- **Delete the vault:** delete the `.sagu` file (and any `.sagu.tmp` file left next to
  it). This deletes all stored connections.
- **Delete the settings:** with the app closed, delete
  `%APPDATA%\SaguTerm\data\app.ron`. In the Microsoft Store version, if the file was
  created by it, the file is in the app's private folder (section 1):
  `%LOCALAPPDATA%\Packages\<SaguTerm folder>\LocalCache\Roaming\SaguTerm\data\app.ron`,
  where the SaguTerm folder is the one with "SaguTerm" in its name. Alternatively, use
  Settings > Apps > Installed apps > SaguTerm > Advanced options > **Reset**, which deletes
  all of the app's private data and may also delete a vault saved inside `AppData`.
- **Clipboard:** clear it in Windows, under Settings > System > Clipboard.
- **Uninstall:** the Store version is removed in Settings > Apps > Installed apps, together
  with the private data Windows kept for it. A vault saved in one of your folders, such as
  Documents, is **not** deleted on uninstall; delete it yourself if you want to. On the
  other hand, in the Store version, a vault created inside `AppData` may be deleted along
  with the app, so prefer to keep the vault in a folder of your own. An executable you
  built yourself is removed by deleting `SaguTerm.exe` and the `%APPDATA%\SaguTerm` folder.
- **Downloaded files** stay in the folder you picked. Delete them there if you want to.
  Uninstalling does not remove what was downloaded to a folder of your own, such as
  Downloads or Documents.
- **Data sent to your servers** stays on those servers. To remove it, contact whoever
  administers them.

## 5. Children

SaguTerm is a technical tool for server administration and is not directed at children.
The app collects no data from anyone, including children.

## 6. Your rights (LGPD)

The developer receives no personal data through SaguTerm and therefore holds no data about
you that could be accessed, corrected or deleted at your request under Brazil's General
Data Protection Law (LGPD, Law No. 13,709/2018). The data kept in the app stays under your
control, on your computer. For data handled by Microsoft, by GitHub or by the
administrators of the servers you connect to, contact those parties.

## 7. Changes to this policy

If the app starts handling data differently, this policy will be updated, with a new
effective date, before or together with the version that brings the change. The change
history is available in this file's history in the repository.

## 8. Contact

Open an issue at <https://github.com/alowelter/sagu-term/issues>. Issues are public: do
not include passwords, keys, server addresses or other personal data.
