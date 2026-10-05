# SaguTerm na Microsoft Store

Roteiro da submissão no Partner Center, com os textos prontos para colar. Os campos estão
com o nome em inglês, como aparecem no Partner Center, e a tradução entre parênteses.
Os limites de tamanho indicados são os da documentação da Microsoft.

## Antes de começar

1. **Identidade do pacote.** No Partner Center, abra o app SaguTerm e vá em
   **Product management > Product identity** (Gerenciamento do produto > Identidade do
   produto). Os três valores já estão em `packaging/msix/identity.json`; confira se batem,
   respeitando maiúsculas e minúsculas:

   | No Partner Center | No `identity.json` |
   |---|---|
   | `Package/Identity/Name` | `name` |
   | `Package/Identity/Publisher` (começa com `CN=`) | `publisher` |
   | `Package/Properties/PublisherDisplayName` | `publisherDisplayName` |

2. **Pacote `.msix`.** Gere o pacote com a identidade real (veja
   [Gerar e testar o pacote](#gerar-e-testar-o-pacote)).
3. **Política de privacidade no ar.** A URL
   `https://github.com/alowelter/sagu-term/blob/master/PRIVACY.md` só funciona depois que o
   `PRIVACY.md` estiver no `master` do GitHub. Abra a URL no navegador antes de enviar.
4. **Capturas de tela.** Pelo menos 1 (recomendado: 4 ou mais). Veja
   [Capturas de tela](#capturas-de-tela).

## Ordem das páginas da submissão

No app, clique em **Start your submission** (Iniciar envio). Preencha as seções nesta
ordem e depois clique em **Submit for certification** (Enviar para certificação):

| Seção do Partner Center | O que fazer |
|---|---|
| **Pricing and availability** (Preço e disponibilidade) | Preço **Free** (gratuito). Mercados e visibilidade podem ficar no padrão. |
| **Properties** (Propriedades) | [Propriedades](#propriedades) |
| **Age ratings** (Classificação etária) | [Questionário de classificação etária](#questionário-de-classificação-etária) |
| **Packages** (Pacotes) | Envie o `.msix`. Deixe marcada só a família **Desktop**. |
| **Store listings** (Listagens da Store) | Idioma **Português (Brasil)**, que vem do pacote: [Página da loja](#página-da-loja-português-brasil) |
| **Submission options** (Opções de envio) | [Capacidade restrita](#capacidade-restrita-runfulltrust) e [Notas para a certificação](#notas-para-a-certificação) |

A certificação costuma levar de algumas horas a alguns dias úteis. Se for recusada, a
Microsoft explica o motivo num relatório, e dá para corrigir e reenviar.

## Propriedades

- **Category** (Categoria): **Developer tools** (Ferramentas de desenvolvedor).
- **Subcategory** (Subcategoria): **Networking** (Rede). Alternativa: **Utilities**.
- **Secondary category** (Categoria secundária): opcional; pode deixar em branco.
- **Privacy policy URL** (URL da política de privacidade):
  `https://github.com/alowelter/sagu-term/blob/master/PRIVACY.md`
- Pergunta sobre acessar, coletar ou transmitir informações pessoais: responda **Sim**. O
  app guarda credenciais e as envia aos servidores do próprio usuário, e apps Win32 sempre
  precisam de política de privacidade (política 10.5.1 da Store).
- **Website** (Site): `https://github.com/alowelter/sagu-term`
- **Support contact info** (Contato de suporte):
  `https://github.com/alowelter/sagu-term/issues`
- **System requirements** (Requisitos do sistema): pode deixar em branco.
- **Product declarations** (Declarações do produto):
  - Instalação em outras unidades ou armazenamento removível: **manter marcado** (padrão).
  - Backup automático dos dados do app no OneDrive: **manter marcado** (padrão).
  - Compras fora do sistema de pagamento da Microsoft: **não marcar** (não há compras).
  - Testado conforme as diretrizes de acessibilidade: **não marcar**, a menos que você
    tenha testado de verdade com o Narrador e com alto contraste.
  - Qualquer declaração sobre IA generativa, caneta ou drivers e serviços: **não marcar**.

## Questionário de classificação etária

Na primeira vez, o Partner Center abre o questionário da IARC, em inglês. A Microsoft
compartilha com a IARC o nome de publisher e o seu e-mail. As respostas são de
responsabilidade do publisher, então responda o que é verdade para o app:

1. **Categoria do app:** escolha a opção de utilitários e produtividade (em inglês, algo
   como *"Utility, Productivity, Communication, or Other"*). Não escolha jogo nem rede
   social.
2. **Conteúdo:** responda **Não** a violência, medo, nudez e sexo, linguagem ofensiva,
   drogas, álcool e tabaco, jogos de azar e humor grosseiro.
3. **Elementos interativos:**
   - Usuários interagem ou trocam conteúdo entre si (chat, voz, rede social)? **Não.** O
     usuário se conecta aos próprios servidores, e não há contato entre usuários do app.
   - Compartilha a localização do usuário? **Não.**
   - Permite comprar bens digitais? **Não.**
   - Acesso irrestrito à internet (navegador ou buscador)? **Não.** O app não é navegador,
     só se conecta aos servidores SSH cadastrados. Apps parecidos na Store (Termius, WinSCP,
     Windows Terminal) também não exibem esse aviso.
   - Se perguntar se compartilha informações pessoais com terceiros: **Não.** As
     credenciais vão só aos servidores do próprio usuário.
4. Clique em **Save and generate** (Salvar e gerar). Resultado esperado: classificação livre
   (IARC 3+, PEGI 3, ESRB Everyone, ClassInd L). A classificação vale também para as
   próximas versões.

## Página da loja (Português (Brasil))

### Product name (Nome do produto)

**SaguTerm** (o nome reservado).

### Description (Descrição)

Obrigatória. Até 10.000 caracteres de texto puro, sem HTML, código ou URLs.

```text
O SaguTerm reúne num só aplicativo o que você usa para administrar servidores Linux e Unix: terminal SSH, navegador de arquivos SFTP e o terminal local do Windows (Prompt de Comando e WSL), lado a lado em painéis divididos na mesma janela.

Suas conexões ficam protegidas num cofre criptografado. Endereços, usuários, senhas e chaves privadas são guardados num arquivo .sagu, no local que você escolher, criptografado com AES-256-GCM e com a chave derivada da sua senha mestra por Argon2id. O arquivo é autossuficiente: leve-o para outro computador e abra-o com a mesma senha.

Tudo foi pensado para o teclado. Digite parte do nome da conexão, aperte Enter e você está conectado. Divida a janela em quantos painéis quiser e troque de painel com Alt+setas. O F1 mostra a lista de atalhos em qualquer tela.

TERMINAL SSH
• Emulação xterm com 256 cores, que se ajusta sozinha ao tamanho do painel.
• Emojis coloridos e símbolos de todo tipo (✓, ✗, ideogramas), desenhados pelo próprio Windows.
• Selecionou, copiou: o texto selecionado vai direto para a área de transferência, e o botão direito cola.
• Autenticação por senha ou por chave privada (formato OpenSSH ou PEM, com passphrase opcional).
• Verificação da chave do servidor, no SSH e no SFTP: na primeira conexão, você confere o tipo e a impressão digital SHA256 da chave e decide se confia. A chave aceita fica guardada no cofre. Se o servidor apresentar outra chave depois, um alerta aparece antes de qualquer senha ser enviada, e cancelar é o padrão.
• Keepalive automático, para a sessão não cair quando fica parada.

NAVEGADOR SFTP
• Navegue pelas pastas do servidor com o teclado ou o mouse. Setas, PageUp, PageDown, Home e End movem na lista, a linha ".." sobe para a pasta acima com Enter (o Backspace também sobe), e digitar as primeiras letras de um nome leva direto ao item.
• Clique na barra do caminho para digitar outro: absoluto, relativo à pasta atual ou com ~ para a sua pasta inicial.
• Pastas e arquivos inconfundíveis: pastas em âmbar, com "/" no fim do nome e sempre no topo da lista. Links simbólicos têm seta no ícone e mostram o destino ao passar o mouse; um link para pasta abre como pasta, e um link quebrado aparece em vermelho.
• Visualizador somente leitura: Enter ou duplo clique num arquivo mostra o conteúdo no próprio painel, com números de linha, busca, seleção e cópia, sem gravar nada no computador. A codificação (UTF-8, UTF-16 com BOM ou Windows-1252) é reconhecida sozinha. Arquivos binários não abrem: aparece um aviso.
• Arraste arquivos do Windows para enviá-los à pasta aberta. Soltos sobre um terminal SSH, eles vão para a pasta atual do shell ou para a pasta que você escolher.
• Baixe arquivos e pastas para o computador: selecione os itens, aperte Ctrl+S ou use o botão de download e escolha a pasta de destino. Pastas vêm com todo o conteúdo.
• O andamento aparece no painel, com botão para cancelar. Se algo já existe no destino, você escolhe entre substituir, pular os existentes ou cancelar.
• Cada arquivo é baixado num temporário e só recebe o nome final quando termina, com a data de modificação do servidor. Nomes que o Windows não aceita são ajustados, e nada é gravado fora da pasta escolhida.
• Renomeie, altere permissões e proprietário/grupo e exclua arquivos e pastas.

TERMINAL LOCAL
• Prompt de Comando e WSL (quando instalado) nos mesmos painéis dos servidores remotos.

PAINÉIS DIVIDIDOS
• Divida qualquer painel lado a lado ou empilhado, quantas vezes quiser, misturando sessões SSH, SFTP e terminais locais.
• Cada painel novo já abre o seletor de conexões com a busca pronta para digitar.

COFRE E CONEXÕES
• Busca instantânea pelo nome da conexão.
• Cadastre, edite e exclua conexões em qualquer seletor.
• Ao conectar, o SaguTerm identifica em segundo plano o sistema do servidor e mostra no cartão o ícone da distribuição (Ubuntu, Debian, Red Hat, AlmaLinux, Rocky Linux, Fedora, openSUSE, FreeBSD e outras). Nada aparece no terminal.
• Ao passar o mouse sobre um cartão, a dica mostra o nome completo da conexão, o endereço, o tipo de autenticação e, quando identificado, o sistema do servidor.
• Ao editar uma conexão, veja a impressão digital da chave do servidor guardada ou esqueça a chave para confirmá-la de novo.
• Bloqueie o cofre na hora com Ctrl+L.
• Abrir sem senha neste computador: ao iniciar, o cofre abre direto, com a chave guardada protegida pela sua conta do Windows. Em outro computador, o arquivo continua pedindo a senha, e bloquear o cofre volta a pedir a senha na próxima abertura.
• O cofre é gravado de forma atômica: uma queda de energia no meio da gravação não corrompe o arquivo.

PRIVACIDADE
Sem conta, sem anúncios e sem telemetria. O SaguTerm não envia nenhum dado ao desenvolvedor e não acessa a internet por conta própria: ele se conecta apenas aos servidores que você cadastra. Para mostrar o ícone do sistema, ele executa no servidor, com o seu usuário, um comando que só lê a identificação do sistema; isso pode ser desligado em cada conexão, e os detalhes estão na política de privacidade. As atualizações chegam pela Microsoft Store.

LIMITAÇÕES ATUAIS
• A interface está em português (Brasil).
• O envio por arrastar e soltar aceita arquivos, mas ainda não pastas.
• O visualizador do SFTP é somente leitura e mostra no máximo os primeiros 4 MiB (e 200 mil linhas) de cada arquivo; para ver o resto, baixe o arquivo.
• No Windows em modo S, o terminal local (Prompt de Comando e WSL) não está disponível, porque o Windows bloqueia esses programas nesse modo. O SSH, o SFTP e o cofre funcionam normalmente.
• Requer Windows 10 ou 11 de 64 bits (x64).

Leve e rápido: escrito em Rust do começo ao fim, incluindo o SSH (sem OpenSSL), com interface acelerada pela placa de vídeo. Código aberto, sob a licença MIT.
```

### What's new in this version (Novidades desta versão)

Limite: 1500 caracteres. Texto da 1.2.0 (648 caracteres), com o abrir sem senha neste
computador e os emojis do terminal:

```text
Abrir sem senha neste computador: marque a opção na tela de conexões e o SaguTerm passa a abrir o cofre direto ao iniciar, sem pedir a senha mestra. A chave do cofre fica guardada protegida pela sua conta do Windows: em outro computador ou em outra conta, o arquivo continua pedindo a senha, e bloquear o cofre (Ctrl+L) faz a próxima abertura pedir a senha de novo.
A senha mestra não fica mais na memória enquanto o cofre está aberto, e salvar alterações no cofre ficou mais rápido.
Terminal: emojis coloridos, como os do seletor do Windows (Win+.), e símbolos que faltavam na fonte, como ✓, ✗ e ideogramas, deixaram de aparecer como quadradinhos.
```

Texto da 1.1.0, só de referência:

```text
Navegador SFTP: copie e mova arquivos e pastas no servidor com Ctrl+C, Ctrl+X e Ctrl+V. Enter num arquivo abre um visualizador somente leitura, com busca e cópia; arquivos binários mostram um aviso. A linha ".." sobe com Enter, PageUp, PageDown, Home, End e as primeiras letras do nome movem na lista, e a barra do caminho aceita digitar outro caminho. Pastas em âmbar com "/" e links simbólicos com seta, com o destino na dica.
Terminal: histórico de rolagem com a roda do mouse ou Shift+PgUp, com seleção e cópia do que já saiu da tela. O btop aparece certo, inclusive os gráficos, as setas funcionam no less e no man, e deixou de sumir a letra depois de um caractere acentuado.
Ao conectar, o app identifica o sistema do servidor e mostra no cartão o ícone da distribuição (Ubuntu, Debian, AlmaLinux e outras), lendo só a identificação do sistema; isso pode ser desligado em cada conexão.
Cartões mais largos, com o nome completo na dica, e a versão no topo da ajuda (F1).
```

Texto da 1.0.1, só de referência:

```text
Abrir o SaguTerm com ele já aberto (atalho de teclado, menu Iniciar ou barra de tarefas) agora traz a janela existente para a frente, restaurando se estiver minimizada, em vez de abrir outra. Assim duas janelas nunca gravam o mesmo cofre, uma por cima da outra.
```

A Microsoft orienta **deixar este campo em branco na primeira submissão**. O texto abaixo
ficou só de referência para a 1.0.0:

```text
Primeira versão na Microsoft Store. Terminal SSH, navegador SFTP com envio de arquivos por arrastar e soltar, terminal local (Prompt de Comando e WSL), painéis divididos e cofre criptografado. Novo nesta versão: download de arquivos e pastas pelo SFTP (Ctrl+S), com andamento, cancelamento e aviso antes de substituir; e verificação da chave do servidor, com confirmação na primeira conexão, chave guardada no cofre e alerta se ela mudar. As atualizações chegam pela própria Store.
```

### Product features (Recursos)

Um recurso por campo, sem marcadores (a Store já coloca). Até 20 itens, com no máximo 200
caracteres cada.

```text
Terminal SSH com emulação xterm de 256 cores e emojis coloridos, que se ajusta ao painel, e keepalive para a sessão não cair
Selecionou, copiou: o texto selecionado vai para a área de transferência, e o botão direito cola
Autenticação por senha ou chave privada (OpenSSH ou PEM, com passphrase opcional)
Verificação da chave do servidor: confira a impressão digital na primeira conexão e receba um alerta se ela mudar
Cofre criptografado com AES-256-GCM e Argon2id, com a opção de abrir sem senha no seu computador (chave protegida pela sua conta do Windows)
Painéis divididos lado a lado ou empilhados, misturando sessões SSH, SFTP e terminais locais
Prompt de Comando e WSL nos mesmos painéis dos servidores remotos
Navegador SFTP: navegue pelas pastas, renomeie, altere permissões e proprietário e exclua arquivos
Visualizador somente leitura no SFTP: Enter num arquivo mostra o conteúdo, com números de linha, busca e cópia; arquivos binários não abrem
SFTP pelo teclado: PageUp/PageDown, Home/End, busca por letras e caminho digitado, com pastas, arquivos e links simbólicos inconfundíveis
Arraste arquivos do Windows para um painel SFTP ou um terminal SSH para enviá-los ao servidor
Baixe arquivos e pastas pelo SFTP com Ctrl+S, com andamento, cancelamento e aviso antes de substituir
Copie e mova arquivos e pastas no servidor com Ctrl+C, Ctrl+X e Ctrl+V no navegador SFTP
Histórico de rolagem: role com a roda do mouse ou Shift+PgUp e copie texto que já saiu da tela
Busca instantânea: digite parte do nome da conexão e aperte Enter
Ícone do sistema de cada servidor (Ubuntu, Debian, Red Hat, AlmaLinux e outros), identificado em segundo plano ao conectar
Feito para o teclado: Alt+setas troca de painel, Ctrl+N cadastra um host, Ctrl+S baixa arquivos, Ctrl+L bloqueia o cofre, F1 mostra os atalhos
Cofre gravado de forma atômica: uma queda de energia durante a gravação não corrompe o arquivo
Escrito em Rust, sem OpenSSL, com interface acelerada pela placa de vídeo
Sem conta, sem anúncios e sem telemetria: suas credenciais ficam no seu computador e só vão para os seus servidores
```

### Short description (Descrição curta)

Opcional, mas recomendada. Até 1000 caracteres; o ideal é ficar abaixo de 270.

```text
Cliente SSH e SFTP com terminal local (Prompt de Comando e WSL) em painéis divididos. Envie e baixe arquivos, confira a chave de cada servidor e guarde hosts, senhas e chaves num cofre criptografado pela sua senha mestra. Tudo se faz pelo teclado.
```

### Keywords (Palavras-chave)

Um termo por campo. Até 7 termos, com no máximo 40 caracteres cada e 21 palavras no total.
Não use nomes de outros produtos (PuTTY, WinSCP, Termius...) nem termos de preço como
"grátis": a política 10.1.3 da Store proíbe.

```text
SSH
SFTP
cliente SSH
terminal
servidores Linux
transferência de arquivos
acesso remoto
```

### Campos adicionais

- **Copyright and trademark info** (Copyright e marca, até 200 caracteres):
  `© 2026 Marcelo Welter. Licença MIT. Ícones de sistemas: Simple Icons (licenças no README). Marcas e logotipos pertencem aos seus donos.`
- **Additional license terms** (Termos de licença adicionais): deixe em branco para usar os
  termos padrão da Store. Se quiser citar a MIT, use a URL
  `https://github.com/alowelter/sagu-term/blob/master/LICENSE`.
- **Developed by** (Desenvolvido por): `Marcelo Welter` (opcional).
- **Additional system requirements** (Requisitos adicionais do sistema): deixe em branco.
- **Short title**, **Sort title**, **Voice title** e trailers: deixe em branco.

### Capturas de tela

Obrigatória 1, recomendadas 4 ou mais, no máximo 10.

- **Formato:** PNG, no mínimo 1366x768 (o ideal é 1920x1080), até 50 MB cada. A legenda de
  cada imagem tem até 200 caracteres.
- **Como capturar:** maximize a janela num monitor Full HD e use a Ferramenta de Captura
  (`Win+Shift+S`, modo Janela), que salva em PNG. Se a imagem sair menor que 1366x768,
  aumente a janela e capture de novo.
- **Nada real à mostra:**
  - Crie um cofre só para as capturas, fora da sua pasta de usuário, por exemplo
    `C:\SaguTerm-demo\demo.sagu`. A tela de conexões mostra o caminho do cofre, e um caminho
    em `C:\Users\...` mostraria o seu nome de usuário. Nunca use o seu cofre real.
  - Cadastre conexões fictícias, como `web-01`, `banco-dados` e `backup`, com endereços
    reservados para documentação (`192.0.2.10`, `198.51.100.20`, `203.0.113.30`) ou
    `example.com`.
  - Para mostrar sessões ao vivo, use um servidor de teste (uma máquina virtual ou o WSL)
    com usuário e nome de máquina neutros, como `demo@web-01`.
  - Não mostre IPs públicos, nomes de clientes, senhas, histórico de comandos com dados
    reais nem notificações da área de trabalho.
- **Sem enfeites:** não sobreponha logos nem textos de marketing. Deixe o essencial nos
  2/3 de cima da imagem.
- **Sem logotipos de terceiros:** a partir da 1.1.0, o cartão de uma conexão já usada mostra
  o ícone da distribuição do servidor (Ubuntu, Debian...). Nas capturas, prefira cartões
  com o ícone padrão de servidor, como os de conexões fictícias que nunca conectaram.

Sugestões de capturas e legendas:

| Captura | Legenda |
|---|---|
| Janela dividida com terminal SSH, navegador SFTP e Prompt de Comando | Vários servidores na mesma janela: terminal SSH, navegador SFTP e Prompt de Comando em painéis divididos. |
| Tela de conexões com os cartões fictícios | Tela de conexões: digite parte do nome e aperte Enter para conectar. |
| Navegador SFTP recebendo arquivos arrastados | Navegador SFTP: arraste arquivos do Windows para enviá-los ao servidor. |
| Navegador SFTP com itens selecionados e o download em andamento no rodapé | Baixe arquivos e pastas do servidor com Ctrl+S, com andamento e cancelamento. |
| Visualizador do SFTP com um arquivo de configuração de teste e a busca aberta | Leia arquivos de texto do servidor sem sair do SaguTerm, com números de linha e busca. |
| Janela "Servidor novo" com a impressão digital da chave de um servidor de teste | Confira a chave do servidor na primeira conexão; ela fica guardada no cofre. |
| Tela do cofre, na aba "Criar cofre" | Cofre criptografado com AES-256-GCM e Argon2id, protegido pela sua senha mestra. |
| Janela de atalhos (F1) | Atalhos de teclado sempre à mão com F1. |

### Store logos (Logotipos da Store)

Envie o **1:1 App tile icon** (ícone 300x300, fortemente recomendado). Para gerá-lo a
partir de `assets/logo.png`, rode no PowerShell, na raiz do repositório:

```powershell
Add-Type -AssemblyName System.Drawing
New-Item -ItemType Directory -Force target\msix | Out-Null
$src = [System.Drawing.Image]::FromFile((Resolve-Path 'assets\logo.png').Path)
$bmp = New-Object System.Drawing.Bitmap 300, 300
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.InterpolationMode = 'HighQualityBicubic'
$g.DrawImage($src, 0, 0, 300, 300)
$bmp.Save("$PWD\target\msix\loja-icone-300.png", [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose(); $src.Dispose()
```

O arquivo sai em `target\msix\loja-icone-300.png`. As artes 2:3 e de caixa são para jogos
e não se aplicam. A arte 16:9 (*Super hero art*, 1920x1080, sem texto e sem mostrar a
interface) é opcional e pode ficar para depois.

## Opções de envio

### Capacidade restrita (runFullTrust)

Em **Submission options > Restricted capabilities** (Opções de envio > Capacidades
restritas), o Partner Center pede uma justificativa para a `runFullTrust`, que todo app
Win32 empacotado precisa declarar. Os testadores da Microsoft leem em inglês, então cole
este texto. O campo é curto: uma versão de 1.365 caracteres não coube, e esta tem 475. Os
detalhes estão nas [Notas para a certificação](#notas-para-a-certificação).

```text
SaguTerm is a classic Win32 desktop app (Rust, egui) packaged as MSIX with the Windows.FullTrustApplication entry point; it cannot run in an AppContainer. Full trust is used only for its core SSH/SFTP client features: outbound TCP connections to servers the user registers, local terminal panes via ConPTY (cmd.exe, wsl.exe), and reading/writing files the user picks (uploads, downloads, SSH keys, encrypted vault). No services, drivers, elevation, telemetry or self-updater.
```

<details>
<summary>Tradução (só para você conferir; não cole)</summary>

O SaguTerm é um app desktop Win32 clássico (Rust, egui), empacotado como MSIX com o ponto
de entrada Windows.FullTrustApplication; ele não roda dentro de um AppContainer. A
confiança total é usada só para as funções principais de cliente SSH/SFTP: conexões TCP de
saída para os servidores que o usuário cadastra, terminais locais pelo ConPTY (cmd.exe,
wsl.exe) e leitura e gravação dos arquivos que o usuário escolhe (envios, downloads, chaves
SSH, cofre criptografado). Sem serviços, drivers, execução como administrador, telemetria
nem atualizador próprio.

</details>

### Notas para a certificação

Em **Submission options > Notes for certification** (Notas para a certificação). Os
testadores são pessoas e precisam conseguir usar o app, então explique o caminho em inglês.
Limite: 2000 caracteres. O texto abaixo tem 1759 (1874 com a linha do servidor de teste,
ainda com os `<...>`), o que deixa folga para os dados reais do servidor; confira a
contagem se mudar alguma coisa:

```text
No account or sign-in is required; there is no backend service, ads, telemetry or in-app purchases. The interface is in Brazilian Portuguese; labels are quoted as they appear in the app.
1. Launch SaguTerm; the splash screen closes after about 2 seconds (or click anywhere).
2. On the vault screen ("Cofre SaguTerm"), select the "Criar cofre" (Create vault) tab, click "Salvar como..." (Save as) and pick any file, e.g. Documents\test.sagu. Type any master password in "Senha mestra" and "Confirmar senha", then click "Criar cofre".
3. Testing without a server: on the "Conexões" (Connections) screen, press Enter or double-click the "Terminal local" card to open a Command Prompt inside the app (a "WSL" card appears if WSL is installed). Ctrl+B then H (side by side) or V (stacked) splits the pane; F1 lists all shortcuts. In S mode Windows blocks cmd.exe and wsl.exe, so these cards show an error instead of a prompt; the vault, split panes and SSH/SFTP work normally.
4. Testing SSH/SFTP: press Ctrl+N ("Novo host"), fill in "Host" (address), "Usuário" (user) and "Senha" (password), or choose "Chave" to paste or load a private key, then click "Salvar". Select the new card and press Enter for an SSH terminal or Ctrl+Enter for the SFTP browser. On the first connection, "Servidor novo" (New server) shows the key fingerprint: click "Confiar e conectar" (Trust and connect).
5. SFTP: files dragged from File Explorer are uploaded to the open folder. To download, select items (Ctrl+click for several), press Ctrl+S and pick a local folder. Enter opens folders, the ".." row goes up, and text files open read-only (Esc returns).
6. Ctrl+L on the Connections screen locks the vault.
No self-updater; the app only connects to the servers the user registers.
```

**Servidor de teste (recomendado).** A política 10.3 pede que o app possa ser testado. Sem
servidor, os testadores conseguem testar o cofre, o terminal local e os painéis, mas não o
SSH/SFTP, que é a função principal. Num PC em modo S, nem o terminal local abre (veja as
notas acima). Se puder, crie um servidor SSH descartável, com um usuário sem privilégios e
só para a certificação, e acrescente ao fim das notas:

```text
Test SSH server (disposable, unprivileged account): address <host>, port <port>, user <user>, password <password>.
```

Troque os `<...>` pelos dados reais, deixe o servidor ligado até a certificação terminar e
apague a conta depois. Se não der para oferecer um servidor, envie sem essa linha.

## Gerar e testar o pacote

Pré-requisitos: Windows SDK (traz o `makeappx.exe` e o `makepri.exe`) e, para o teste
local, o **Modo de Desenvolvedor** do Windows ligado. Rode tudo na raiz do repositório. Sem
o PowerShell 7 (`pwsh`), use `powershell -ExecutionPolicy Bypass -File` no lugar de
`pwsh -File`.

### Teste local, com a identidade de teste

Não precisa do `identity.json` preenchido.

```powershell
cargo build --release
pwsh -File packaging\msix\build.ps1 -TestIdentity
Add-AppxPackage -Register target\msix\layout\AppxManifest.xml
```

Abra o **SaguTerm** pelo menu Iniciar e confira:

- no F1, o topo da ajuda mostra `SaguTerm` e `versão <versão>`, a mesma do `Cargo.toml`, e o
  título da janela é só `SaguTerm`;
- com o app de teste aberto, abri-lo de novo pelo menu Iniciar traz a mesma janela para a
  frente, sem abrir outra. Se a versão da Store estiver aberta, abrir o app de teste traz a
  janela dela para a frente, como na troca de versão pela Store; feche-a antes de seguir;
- o cofre, o terminal local e uma conexão SSH funcionam como no executável avulso
  (`target\release\SaguTerm.exe`), inclusive a janela "Servidor novo" na primeira conexão;
- na tela de conexões, marque **Abrir sem senha neste computador**, feche o app e abra de
  novo pelo menu Iniciar: o cofre abre direto, sem pedir a senha. Depois, `Ctrl+L`, feche e
  abra: a senha é pedida de novo, e digitá-la religa a abertura automática;
- deixe uma sessão SSH ou SFTP aberta por uns 5 segundos num servidor de teste (por
  exemplo, uma distribuição do WSL com o `sshd` ligado). Depois disso, o cartão dessa
  conexão (na tela de conexões ou no seletor de um painel novo) mostra o ícone da
  distribuição, e a dica mostra o nome completo, o endereço, o tipo de autenticação e a
  linha `Sistema: <nome> <versão>`. Nada da detecção aparece no terminal. Um servidor sem
  ícone no SaguTerm (como Oracle Linux ou Kali) mantém o ícone padrão de servidor;
- no editor dessa conexão, desmarcar **Detectar o sistema do servidor** e salvar volta o
  cartão ao ícone de servidor, e a dica deixa de mostrar a linha `Sistema:`. Marcar de novo
  e reconectar traz o ícone de volta;
- o ícone de chave ou senha aparece à esquerda do endereço, no cartão;
- num painel SFTP do servidor de teste, as setas, `PageUp`/`PageDown` e `Home`/`End` movem
  o cursor, inclusive até a linha `..`, que sobe para a pasta acima com `Enter` ou duplo
  clique. O `Backspace` também sobe, e a pasta de onde se saiu fica selecionada. Digitar as
  primeiras letras de um nome leva ao item, com o indicador no canto da lista;
- clicar em qualquer ponto da barra do caminho abre a edição com o texto selecionado. `~` e
  `Enter` vão à pasta inicial; um caminho que não existe mostra `Caminho não encontrado`
  embaixo da barra, sem fechar a edição; `Esc` cancela;
- `Enter` num arquivo de texto abre o visualizador, com números de linha e a codificação no
  cabeçalho; `Ctrl+F` busca, e `Esc` volta à listagem no mesmo item. `Enter` num binário
  (por exemplo, `/usr/bin/ls`) mostra o aviso de arquivo binário e não abre nada;
- no servidor de teste, crie na pasta inicial `ln -s /etc link-pasta` e
  `ln -s /nao/existe link-quebrado` e atualize o SFTP (`F5`): `link-pasta/` aparece junto
  das pastas, em âmbar e com seta no ícone, e abre com `Enter`; `link-quebrado` aparece em
  vermelho, e `Enter` sobre ele mostra um aviso. Depois, apague os dois com
  `rm link-pasta link-quebrado`;
- um download pelo SFTP (`Ctrl+S`) para a pasta Downloads aparece nessa pasta quando você
  a abre pelo Explorador de Arquivos, fora do SaguTerm;
- no SFTP, `Ctrl+C` num arquivo mostra a faixa `Copiando` no topo do painel; `Ctrl+V` em
  outra pasta cria a cópia, e na mesma pasta cria `nome (cópia)`. `Ctrl+X` e `Ctrl+V` em
  outra pasta movem o arquivo (a faixa some); um nome que já existe abre a janela `Já
  existe no destino`. `Esc` desiste antes de colar;
- num terminal SSH, `seq 1 20000` e depois a roda do mouse, `Shift+PgUp` e `Shift+End`
  rolam o histórico; digitar volta ao fim. O `man ls` rola com as setas;
- os programas abertos no terminal local gravam no registro e nas pastas normais do
  Windows, e não na pasta privada do app. No **Terminal local** do SaguTerm, rode:

  ```bat
  reg add HKCU\Software\SaguTermTeste /v teste /d 1 /f
  mkdir %APPDATA%\sagu-teste
  ```

  Depois, num Prompt de Comando aberto pelo menu Iniciar (fora do SaguTerm), rode os
  dois comandos abaixo. Os dois devem encontrar o que foi criado:

  ```bat
  reg query HKCU\Software\SaguTermTeste
  dir %APPDATA%\sagu-teste
  ```

  Para apagar o teste: `reg delete HKCU\Software\SaguTermTeste /f` e
  `rmdir %APPDATA%\sagu-teste`. Num teste no Windows 11 (build 26200), com um pacote que
  abre o cmd.exe do mesmo jeito que o SaguTerm, o resultado foi esse: só o processo
  empacotado gravou na pasta privada, e o cmd.exe aberto por ele gravou nos lugares
  normais. Se aqui os comandos não encontrarem o que foi criado, o Windows
  está desviando as gravações do terminal local. Nesse caso, comandos como `setx` e
  `pip install --user` não valeriam fora do SaguTerm e sumiriam ao desinstalar, e o
  pacote não deve ir para a Store antes de rever o manifesto.

Para remover o app de teste (faça isso antes de gerar o pacote de novo):

```powershell
Get-AppxPackage SaguTerm.Dev | Remove-AppxPackage
```

### Pacote para enviar à Store

Com o `identity.json` preenchido:

```powershell
cargo build --release
pwsh -File packaging\msix\build.ps1
```

O pacote sai em `target\msix\SaguTerm-<versão>-x64.msix`. É ele que vai na seção
**Packages** do Partner Center. O pacote não é assinado: a Store assina na publicação.
Por isso, ele não instala com um clique duplo no seu PC. Para testar, use o
`Add-AppxPackage -Register` da seção anterior.

Se registrou o pacote com a identidade real, remova esse registro antes de instalar o
SaguTerm pela Store. Ele tem a mesma identidade do app da Store e entraria em conflito
com a instalação. Na raiz do repositório:

```powershell
Get-AppxPackage (Get-Content packaging\msix\identity.json -Raw | ConvertFrom-Json).name | Remove-AppxPackage
```

O `.msix` é gerado só no seu PC, com o `build.ps1`. O GitHub guarda apenas o código-fonte,
e o app é distribuído só pela Microsoft Store.

Opcional: o Windows App Certification Kit roda localmente alguns dos testes técnicos da
certificação. Ele testa um app já instalado, então registre antes o layout com
`Add-AppxPackage -Register`, como no teste local. Depois, num PowerShell aberto como
administrador, na raiz do repositório:

```powershell
$kit = "C:\Program Files (x86)\Windows Kits\10\App Certification Kit\appcert.exe"
$pacote = (Get-AppxPackage SaguTerm.Dev).PackageFullName
Remove-Item target\msix\wack.xml -ErrorAction SilentlyContinue
& $kit reset
& $kit test -packagefullname $pacote -reportoutputpath "$PWD\target\msix\wack.xml"
```

Se registrou com a identidade real, troque `SaguTerm.Dev` pelo `name` do
`identity.json`. O relatório sai em `target\msix\wack.xml`. Se o kit não rodar, siga em
frente: a certificação da Store faz as mesmas verificações e diz o que corrigir.

### Próximas versões

1. Suba o `version` do `Cargo.toml` (a versão da Store fica `X.Y.Z.0`, e a primeira parte
   não pode ser 0).
2. Gere o pacote novo com `cargo build --release` e `pwsh -File packaging\msix\build.ps1`,
   como em [Pacote para enviar à Store](#pacote-para-enviar-à-store).
3. No Partner Center, crie uma nova submissão, troque o pacote em **Packages** pelo `.msix`
   novo e preencha **What's new in this version** com as novidades.
