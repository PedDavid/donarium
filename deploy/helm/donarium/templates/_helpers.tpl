{{- define "donarium.name" -}}
{{- .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "donarium.fullname" -}}
{{- if contains .Chart.Name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "donarium.labels" -}}
app.kubernetes.io/name: {{ include "donarium.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end -}}

{{- define "donarium.selectorLabels" -}}
app.kubernetes.io/name: {{ include "donarium.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "donarium.cookieSecretName" -}}
{{- default (printf "%s-cookie-key" (include "donarium.fullname" .)) .Values.cookieKey.existingSecret -}}
{{- end -}}

{{- define "donarium.claimName" -}}
{{- default (printf "%s-data" (include "donarium.fullname" .)) .Values.persistence.existingClaim -}}
{{- end -}}
