; 苛读 · NSIS 安装钩子：用自定义 ICO 覆盖文件关联 DefaultIcon
;
; Tauri 默认 APP_ASSOCIATE 把图标写成 `$INSTDIR\<exe>,0`（程序图标）。
; 此处在 POSTINSTALL 阶段改成随安装包分发的独立 .ico，与资源管理器观感一致。
;
; ProgID 来自 tauri.conf.json → fileAssociations[].name。

!macro NSIS_HOOK_POSTINSTALL
  ; icons 已通过 bundle.resources 装到 $INSTDIR
  WriteRegStr SHELL_CONTEXT "Software\Classes\EPUB\DefaultIcon" "" "$INSTDIR\file-epub.ico"
  WriteRegStr SHELL_CONTEXT "Software\Classes\MOBI\DefaultIcon" "" "$INSTDIR\file-mobi.ico"
  WriteRegStr SHELL_CONTEXT "Software\Classes\AZW3\DefaultIcon" "" "$INSTDIR\file-azw3.ico"
  WriteRegStr SHELL_CONTEXT "Software\Classes\FB2\DefaultIcon" "" "$INSTDIR\file-fb2.ico"
  WriteRegStr SHELL_CONTEXT "Software\Classes\CBZ\DefaultIcon" "" "$INSTDIR\file-cbz.ico"
  ; 通知资源管理器刷新关联图标
  !insertmacro UPDATEFILEASSOC
!macroend
