{{/*
"sidecarzero.appEnv": the host container's environment, and "sidecarzero.appVolumeMounts" its
mount of the socket's directory. The pod also needs an fsGroup both containers share
(kubernetes-deploy §7): "sidecarzero.fsGroup" is the sidecar's group.

  containers:
    - name: app
      env:
        {{- include "sidecarzero.appEnv" . | nindent 8 }}
      volumeMounts:
        {{- include "sidecarzero.appVolumeMounts" . | nindent 8 }}
*/}}
{{- define "sidecarzero.appEnv" -}}
{{- $v := .Values.sidecarzero -}}
- name: SIDECARZERO_SOCKET
  value: {{ include "sidecarzero.endpoint" . | quote }}
{{- end -}}

{{/* With socket.kind=abstract there is nothing to mount: it renders an empty list. */}}
{{- define "sidecarzero.appVolumeMounts" -}}
{{- $v := .Values.sidecarzero -}}
{{- if eq $v.socket.kind "path" -}}
- { name: scz-run, mountPath: {{ $v.socket.dir }} }
{{- else -}}
[]
{{- end -}}
{{- end -}}

{{- define "sidecarzero.fsGroup" -}}
{{- .Values.sidecarzero.securityContext.runAsGroup | default 65532 -}}
{{- end -}}
