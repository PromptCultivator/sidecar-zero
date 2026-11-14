{{/*
"sidecarzero.sidecar": the sidecar container. With sidecar.mode=native, include it under the
pod's initContainers, after any init container that fetches the model; with classic, under
containers.

  initContainers:
    {{- include "sidecarzero.sidecar" . | nindent 4 }}
*/}}
{{- define "sidecarzero.sidecar" -}}
{{- $v := .Values.sidecarzero -}}
- name: {{ $v.sidecar.name }}
  image: {{ include "sidecarzero.image" . }}
  imagePullPolicy: {{ $v.image.pullPolicy }}
  {{- if eq $v.sidecar.mode "native" }}
  restartPolicy: Always
  {{- end }}
  args:
    {{- include "sidecarzero.args" . | nindent 4 }}
  {{- if not $v.model.prepack }}
  env:
    # Workers inherit the supervisor's environment; the ONNX Runtime worker reads this.
    - { name: SCZ_ORT_PREPACK, value: "0" }
  {{- end }}
  ports:
    - { name: scz-http, containerPort: {{ $v.http.port }} }
  startupProbe:
    httpGet: { path: /startupz, port: scz-http }
    periodSeconds: 1
    failureThreshold: {{ if $v.startup.waitForModel }}{{ add 30 $v.worker.loadTimeoutSeconds }}{{ else }}30{{ end }}
  readinessProbe:
    httpGet: { path: /readyz, port: scz-http }
    periodSeconds: 2
  livenessProbe:
    httpGet: { path: /livez, port: scz-http }
    periodSeconds: 5
    failureThreshold: 3
  resources:
    {{- toYaml $v.resources | nindent 4 }}
  securityContext:
    {{- include "sidecarzero.securityContext" . | nindent 4 }}
  volumeMounts:
    {{- if eq $v.socket.kind "path" }}
    - { name: scz-run, mountPath: {{ $v.socket.dir }} }
    {{- end }}
    - { name: scz-admin, mountPath: {{ $v.adminDir }} }
    {{- if $v.model.volume }}
    - { name: scz-models, mountPath: {{ $v.model.mountPath }}, readOnly: true }
    {{- end }}
{{- end -}}
