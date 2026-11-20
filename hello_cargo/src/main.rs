use std::{io, path::Path};

use axum::{
    Router,
    body::Body,
    extract::{DefaultBodyLimit, Multipart, multipart::MultipartError},
    http::{StatusCode, header},
    response::{Html, IntoResponse},
    routing::{get, post},
};
use futures_util::{StreamExt, stream};
use tokio::{fs::File, io::AsyncWriteExt, net::TcpListener, process::Command};
use tokio_util::io::ReaderStream;

const MAX_EXPORT_REQUEST_SIZE: usize = 100 * 1024 * 1024;
type RequestError = (StatusCode, String);

#[tokio::main]
async fn main() -> io::Result<()> {
    let app = Router::new()
        .route("/", get(index))
        .route("/editor.js", get(editor_script))
        .route(
            "/export",
            post(export).layer(DefaultBodyLimit::max(MAX_EXPORT_REQUEST_SIZE)),
        )
        .route(
            "/concat",
            post(concat).layer(DefaultBodyLimit::max(MAX_EXPORT_REQUEST_SIZE)),
        );

    let listener = TcpListener::bind("127.0.0.1:3000").await?;
    println!("Открой http://localhost:3000 в браузере");

    axum::serve(listener, app).await
}

async fn index() -> Html<&'static str> {
    Html(include_str!("web/index.html"))
}

async fn editor_script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("web/editor.js"),
    )
}

async fn export(mut form: Multipart) -> Result<impl IntoResponse, RequestError> {
    let work_dir = tempfile::tempdir().map_err(file_error)?;
    let input_path = work_dir.path().join("input.mp4");
    let output_path = work_dir.path().join("output.mp4");
    let mut start = None;
    let mut end = None;
    let mut received_video = false;

    while let Some(mut field) = form.next_field().await.map_err(form_error)? {
        match field.name() {
            Some("video") => {
                if received_video {
                    return Err((StatusCode::BAD_REQUEST, "Отправлено несколько видео".into()));
                }
                if field.file_name().is_none() {
                    return Err((StatusCode::BAD_REQUEST, "Выбери видеофайл".into()));
                }

                let mut input = File::create(&input_path).await.map_err(file_error)?;
                let mut size = 0;
                while let Some(chunk) = field.chunk().await.map_err(form_error)? {
                    size += chunk.len();
                    input.write_all(&chunk).await.map_err(file_error)?;
                }
                input.flush().await.map_err(file_error)?;
                if size == 0 {
                    return Err((StatusCode::BAD_REQUEST, "Видео содержит 0 байт".into()));
                }
                received_video = true;
            }
            Some("start") => {
                start = Some(parse_time(&field.text().await.map_err(form_error)?)?);
            }
            Some("end") => {
                end = Some(parse_time(&field.text().await.map_err(form_error)?)?);
            }
            _ => {}
        }
    }

    if !received_video {
        return Err((StatusCode::BAD_REQUEST, "Поле video отсутствует".into()));
    }

    let start =
        start.ok_or_else(|| (StatusCode::BAD_REQUEST, "Не задано начало фрагмента".into()))?;
    let end = end.ok_or_else(|| (StatusCode::BAD_REQUEST, "Не задан конец фрагмента".into()))?;
    if start >= end {
        return Err((
            StatusCode::BAD_REQUEST,
            "Конец фрагмента должен быть позже начала".into(),
        ));
    }

    trim_video(&input_path, &output_path, start, end - start).await?;

    let output = File::open(&output_path).await.map_err(file_error)?;
    let size = output.metadata().await.map_err(file_error)?.len();
    let reader = ReaderStream::with_capacity(output, 64 * 1024);
    // Папка остаётся открытой до окончания или отмены скачивания.
    let download = stream::try_unfold((reader, work_dir), |(mut reader, work_dir)| async move {
        match reader.next().await {
            Some(chunk) => Ok(Some((chunk?, (reader, work_dir)))),
            None => Ok::<_, io::Error>(None),
        }
    });

    let headers = [
        (header::CONTENT_TYPE, "video/mp4".to_owned()),
        (
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"video-trimmed.mp4\"".to_owned(),
        ),
        (header::CONTENT_LENGTH, size.to_string()),
    ];
    Ok((headers, Body::from_stream(download)))
}

async fn trim_video(
    input_path: &Path,
    output_path: &Path,
    start: f64,
    duration: f64,
) -> Result<(), RequestError> {
    let start = format!("{start:.6}");
    let duration = format!("{duration:.6}");
    let result = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-abort_on",
            "empty_output",
            "-protocol_whitelist",
            "file",
            "-f",
            "mov",
            "-ss",
            &start,
            "-i",
        ])
        .arg(input_path)
        .args(["-t", &duration, "-map", "0:V:0", "-map", "0:a:0?"])
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "23",
            "-vf",
            "pad=ceil(iw/2)*2:ceil(ih/2)*2",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-movflags",
            "+faststart",
        ])
        .arg(output_path)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| {
            eprintln!("Не удалось запустить FFmpeg: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Не удалось запустить FFmpeg. Проверь, что он установлен.".into(),
            )
        })?;

    if !result.status.success() {
        let details = String::from_utf8_lossy(&result.stderr);
        eprintln!("FFmpeg завершился с ошибкой: {details}");
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "Не удалось обрезать видео. Проверь файл MP4 и границы фрагмента.".into(),
        ));
    }

    Ok(())
}

async fn concat(mut form: Multipart) -> Result<impl IntoResponse, RequestError> {
    let work_dir = tempfile::tempdir().map_err(file_error);
}

fn parse_time(value: &str) -> Result<f64, RequestError> {
    let seconds = value.parse::<f64>().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "Время начала и конца должно быть числом".into(),
        )
    })?;

    if !seconds.is_finite() || seconds < 0.0 {
        return Err((
            StatusCode::BAD_REQUEST,
            "Время должно быть конечным неотрицательным числом".into(),
        ));
    }

    Ok(seconds)
}

fn form_error(error: MultipartError) -> RequestError {
    (error.status(), error.body_text())
}

fn file_error(error: io::Error) -> RequestError {
    eprintln!("Ошибка временного файла: {error}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Не удалось прочитать или сохранить видео".into(),
    )
}
