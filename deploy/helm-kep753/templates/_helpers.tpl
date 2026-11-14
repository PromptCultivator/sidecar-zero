{{/* Every named template takes the including chart's root context and reads its `sidecarzero:` values. */}}

{{- define "sidecarzero.image" -}}
{{- $v := .Values.sidecarzero -}}
{{- printf "%s:%s" $v.image.repository (default .Chart.AppVersion $v.image.tag) -}}
{{- end -}}

{{/* The endpoint both containers use: a path in the shared directory, or an abstract name. */}}
{{- define "sidecarzero.endpoint" -}}
{{- $v := .Values.sidecarzero -}}
{{- if eq $v.socket.kind "abstract" -}}
@{{ $v.socket.name }}
{{- else if eq $v.socket.kind "path" -}}
{{ $v.socket.dir }}/{{ $v.socket.name }}
{{- else -}}
{{- fail (printf "sidecarzero.socket.kind must be path or abstract, not %q" $v.socket.kind) -}}
{{- end -}}
{{- end -}}

{{- define "sidecarzero.args" -}}
{{- $v := .Values.sidecarzero -}}
- --socket={{ include "sidecarzero.endpoint" . }}
- --admin={{ $v.adminDir }}/admin.sock
- --http=0.0.0.0:{{ $v.http.port }}
- --model={{ $v.model.path }}
{{- with $v.model.sha256 }}
- --model-sha256={{ . }}
{{- end }}
{{- $modelCopy := $v.model.copy | default "none" }}
{{- if ne $modelCopy "none" }}
- --model-copy={{ $modelCopy }}
{{- end }}
- --standby={{ $v.worker.standby }}
- --rings={{ $v.worker.threads }}
- --ring-capacity={{ $v.arena.ringCapacity }}
- --slots={{ $v.arena.small.slots }}
- --slot-size={{ mul $v.arena.small.slotSizeKi 1024 }}
{{- if gt (int $v.arena.batch.slots) 0 }}
- --batch-slots={{ $v.arena.batch.slots }}
- --batch-slot-size={{ mul $v.arena.batch.slotSizeKi 1024 }}
{{- end }}
- --spin-us={{ $v.worker.spinMicros }}
- --load-timeout-ms={{ mul $v.worker.loadTimeoutSeconds 1000 }}
- --max-deadline-ms={{ $v.worker.maxDeadlineMillis }}
{{- if $v.worker.coreDumps }}
- --core-dumps
{{- end }}
{{- if gt (int $v.worker.rlimitDataMi) 0 }}
- --worker-rlimit-data-mb={{ $v.worker.rlimitDataMi }}
{{- end }}
- --rollback={{ $v.reload.rollback }}
- --rollback-window-ms={{ mul $v.reload.rollbackWindowSeconds 1000 }}
- --fail-open={{ $v.startup.failOpen }}
{{- if $v.startup.waitForModel }}
- --wait-for-model
{{- end }}
{{- if and $v.hugepages $v.hugepages.enabled }}
- --hugepages
{{- end }}
- --oom-shield={{ $v.oomShield.mode }}
{{- if gt (int $v.oomShield.workerMemoryMaxMi) 0 }}
- --worker-memory-max-mb={{ $v.oomShield.workerMemoryMaxMi }}
{{- end }}
- --session-grace-ms={{ mul $v.shutdown.sessionGraceSeconds 1000 }}
- --pressure-shed={{ $v.pressure.shed }}
- --pressure-recover={{ $v.pressure.recover }}
- --pressure-hard={{ $v.pressure.hard }}
- --pressure-shed-max-ms={{ mulf $v.pressure.shedMaxSeconds 1000 | int }}
{{- range $v.extraArgs }}
- {{ . }}
{{- end }}
{{- end -}}

{{- define "sidecarzero.securityContext" -}}
{{- $v := .Values.sidecarzero -}}
{{- if eq $v.oomShield.mode "oomScoreAdj" -}}
# oomScoreAdj needs CAP_SYS_RESOURCE in effect, which a non-root container cannot have
# (kubernetes-deploy §5): root, with every other capability dropped. A privileged namespace.
runAsUser: 0
runAsNonRoot: false
readOnlyRootFilesystem: true
allowPrivilegeEscalation: false
capabilities: { drop: ["ALL"], add: ["SYS_RESOURCE"] }
seccompProfile: { type: RuntimeDefault }
{{- else if $v.privileged -}}
privileged: true
runAsUser: 0
readOnlyRootFilesystem: true
{{- else -}}
{{ toYaml $v.securityContext }}
{{- end -}}
{{- end -}}
