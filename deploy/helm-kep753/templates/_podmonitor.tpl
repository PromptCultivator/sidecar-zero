{{/*
"sidecarzero.podMonitor": a Prometheus Operator PodMonitor for the sidecar's metrics, rendered
only with podMonitor.enabled. A library chart renders no manifests of its own, so the
including chart emits it:

  {{ include "sidecarzero.podMonitor" (dict "root" . "selector" (dict "app" "my-service")) }}
*/}}
{{- define "sidecarzero.podMonitor" -}}
{{- $v := .root.Values.sidecarzero -}}
{{- if $v.podMonitor.enabled }}
apiVersion: monitoring.coreos.com/v1
kind: PodMonitor
metadata:
  name: {{ .root.Release.Name }}-sidecarzero
spec:
  selector:
    matchLabels:
      {{- toYaml .selector | nindent 6 }}
  podMetricsEndpoints:
    - port: scz-http
      path: /metrics
      interval: {{ $v.podMonitor.interval }}
{{- end }}
{{- end -}}
