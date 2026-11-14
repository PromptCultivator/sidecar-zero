{{/*
"sidecarzero.volumes": the pod's volumes for the sidecar: the socket's directory, shared with
the host (none with socket.kind=abstract); the admin socket's, which only the sidecar mounts;
and the model's, if one is set.

  volumes:
    {{- include "sidecarzero.volumes" . | nindent 4 }}
*/}}
{{- define "sidecarzero.volumes" -}}
{{- $v := .Values.sidecarzero -}}
{{- if eq $v.socket.kind "path" }}
- name: scz-run
  emptyDir: {}
{{- end }}
- name: scz-admin
  emptyDir: {}
{{- with $v.model.volume }}
- name: scz-models
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- end -}}
