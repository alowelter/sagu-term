# Política de Privacidade do SaguTerm

**Vigência:** 25 de setembro de 2026 · [English version below](#english)

Esta política vale para o SaguTerm distribuído pela **Microsoft Store** e também para um
executável que você mesmo compile a partir do código-fonte, que é público no GitHub. O
SaguTerm é um cliente SSH/SFTP com terminal local para Windows, de código aberto (licença
MIT), mantido por Marcelo Welter ("o desenvolvedor").

## Resumo

- **O SaguTerm não envia nenhum dado ao desenvolvedor.** Não há conta, cadastro, anúncios,
  telemetria, estatísticas de uso nem relatórios de erro próprios, e o app não grava
  registros (logs) em disco.
- Suas conexões, senhas e chaves, e também as chaves públicas dos servidores que você
  aceitou, ficam num **cofre criptografado**, num arquivo guardado onde você escolher.
- Os arquivos que você baixa dos seus servidores vão só para a pasta que você escolhe.
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

- Todo o conteúdo é criptografado com **AES-256-GCM**. A chave de 256 bits é derivada da
  sua senha mestra com **Argon2id**, e um *salt* e um *nonce* aleatórios novos são gerados a
  cada gravação.
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

### Na memória, enquanto o cofre está aberto

Para poder salvar as suas alterações, o app mantém a senha mestra e o conteúdo do cofre na
memória enquanto o cofre está aberto. Ao bloquear o cofre, o app descarta esses dados, mas
não sobrescreve a memória que eles ocupavam. Só a chave derivada da senha é apagada da
memória ativamente, logo após cada uso.

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

- Ao terminar de selecionar texto no terminal com o mouse, o texto selecionado é
  **copiado automaticamente** para a área de transferência do Windows. Cuidado ao
  selecionar senhas ou outros dados sensíveis.
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
  dono, grupo e permissões) e os arquivos `/etc/passwd` e `/etc/group`, para mostrar nomes
  de usuários e grupos em vez de números. Tudo isso fica só na memória;
- os arquivos e pastas que você manda baixar (conteúdo, tamanho e data de modificação),
  gravados na pasta que você escolheu (seção 1);
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
- **Bloquear o cofre:** `Ctrl+L` na tela de conexões ou o botão **Bloquear cofre**.
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

**Effective date:** September 25, 2026

This policy applies to SaguTerm distributed through the **Microsoft Store** and also to an
executable you build yourself from the source code, which is public on GitHub. SaguTerm is
an open source (MIT license) SSH/SFTP client with a local terminal for Windows, maintained
by Marcelo Welter ("the developer"). If the Portuguese and English versions differ, the
Portuguese version prevails.

## Summary

- **SaguTerm sends no data to the developer.** There is no account, sign-up, ads,
  telemetry, usage statistics or crash reporting of its own, and the app writes no logs to
  disk.
- Your connections, passwords and keys, as well as the public keys of the servers you
  accepted, are kept in an **encrypted vault**, in a file stored wherever you choose.
- Files you download from your servers go only to the folder you choose.
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

- All of its content is encrypted with **AES-256-GCM**. The 256-bit key is derived from
  your master password with **Argon2id**, and a new random salt and nonce are generated on
  every save.
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

### In memory, while the vault is open

To be able to save your changes, the app keeps the master password and the vault content
in memory while the vault is open. When you lock the vault, the app discards this data but
does not overwrite the memory it used. Only the key derived from the password is actively
wiped from memory, right after each use.

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

- When you finish selecting text in the terminal with the mouse, the selected text is
  **automatically copied** to the Windows clipboard. Be careful when selecting passwords or
  other sensitive data.
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
  group and permissions) and the `/etc/passwd` and `/etc/group` files, to show user and
  group names instead of numbers. All of this stays in memory only;
- the files and folders you choose to download (content, size and modification date),
  written to the folder you picked (section 1);
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
- **Lock the vault:** `Ctrl+L` on the connections screen, or the **Bloquear cofre** (Lock
  vault) button.
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
