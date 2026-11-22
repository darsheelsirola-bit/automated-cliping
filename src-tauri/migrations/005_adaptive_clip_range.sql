UPDATE app_settings SET value=json_set(value,'$.minClipSeconds',10,'$.maxClipSeconds',45)
WHERE key='preferences' AND json_extract(value,'$.minClipSeconds')=25
AND json_extract(value,'$.maxClipSeconds')=60;
