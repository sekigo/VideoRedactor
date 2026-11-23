use std::{
    io,
    path::{Path, PathBuf},
};

use axum::{
    Router,
    body::Body,
    extract::{DefaultBodyLimit, Multipart, multipart::MultipartError},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use futures_util::{StreamExt, stream};
use serde_json::Value;
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
    download_video(work_dir, &output_path, "video-trimmed.mp4").await
}

async fn download_video(
    work_dir: tempfile::TempDir,
    output_path: &Path,
    filename: &str,
) -> Result<Response, RequestError> {
    let output = File::open(output_path).await.map_err(file_error)?;
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
            format!("attachment; filename=\"{filename}\""),
        ),
        (header::CONTENT_LENGTH, size.to_string()),
    ];
    Ok((headers, Body::from_stream(download)).into_response())
}

async fn concat(mut form: Multipart) -> Result<impl IntoResponse, RequestError> {
    let work_dir = tempfile::tempdir().map_err(file_error)?;
    let output_path = work_dir.path().join("joined.mp4");
    let mut input_paths = Vec::new();

    while let Some(mut field) = form.next_field().await.map_err(form_error)? {
        if !matches!(field.name(), Some("videos" | "first" | "second")) {
            continue;
        }

        if field.file_name().is_none_or(str::is_empty) {
            return Err((StatusCode::BAD_REQUEST, "Выбери видеофайл".into()));
        }

        let name = format!("clip-{}.mp4", input_paths.len() + 1);
        let input_path = work_dir.path().join(name);
        let mut file = File::create(&input_path).await.map_err(file_error)?;
        let mut size = 0;

        while let Some(chunk) = field.chunk().await.map_err(form_error)? {
            size += chunk.len();
            file.write_all(&chunk).await.map_err(file_error)?;
        }

        file.flush().await.map_err(file_error)?;

        if size == 0 {
            return Err((StatusCode::BAD_REQUEST, "Видео содержит 0 байт".into()));
        }

        input_paths.push(input_path);
    }

    if input_paths.len() < 2 {
        return Err((StatusCode::BAD_REQUEST, "Выбери хотя бы два видео".into()));
    }

    concat_video(&input_paths, &output_path).await?;
    download_video(work_dir, &output_path, "video-joined.mp4").await
}

async fn concat_video(input_paths: &[PathBuf], output_path: &Path) -> Result<(), RequestError> {
    let mut clips = Vec::with_capacity(input_paths.len());
    let mut command = Command::new("ffmpeg");
    command.args([
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-xerror",
        "-abort_on",
        "empty_output",
        "-filter_complex_threads",
        "1",
    ]);
    for (index, path) in input_paths.iter().enumerate() {
        clips.push(probe_clip(path, index + 1).await?);
        command.args(["-protocol_whitelist", "file", "-f", "mov", "-i"]);
        command.arg(path);
    }

    let (width, height) = clips[0].canvas_size();
    let has_audio = clips.iter().any(|clip| clip.audio_stream.is_some());
    let mut filters = Vec::new();
    let mut segments = String::new();
    for (index, clip) in clips.iter().enumerate() {
        let video = clip.video_stream;
        // dar учитывает пропорции пикселей. Pad добавляет поля, сохраняя пропорции кадра.
        filters.push(format!(
            "[{index}:{video}]setpts=PTS-STARTPTS,fps=30,\
             scale=w='max(2,trunc(min({width},{height}*dar)/2)*2)':\
             h='max(2,trunc(min({height},{width}/dar)/2)*2)',\
             setsar=1,format=yuv420p,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2[v{index}]"
        ));
        segments.push_str(&format!("[v{index}]"));

        if has_audio {
            let duration = clip.duration;
            let audio = match clip.audio_stream {
                Some(stream) => format!(
                    "[{index}:{stream}]asetpts=PTS-STARTPTS+({:.6})/TB,\
                     aresample=48000:async=1:first_pts=0,aformat=channel_layouts=stereo,\
                     apad,atrim=duration={duration:.6}[a{index}]",
                    clip.audio_offset
                ),
                None => format!("anullsrc=r=48000:cl=stereo:d={duration:.6}[a{index}]"),
            };
            filters.push(audio);
            segments.push_str(&format!("[a{index}]"));
        }
    }

    let audio_output = if has_audio { "1[v][a]" } else { "0[v]" };
    filters.push(format!(
        "{segments}concat=n={}:v=1:a={audio_output}",
        clips.len()
    ));
    command.arg("-filter_complex").arg(filters.join(";"));
    command.args([
        "-map", "[v]", "-c:v", "libx264", "-preset", "veryfast", "-crf", "23",
    ]);
    if has_audio {
        command.args(["-map", "[a]", "-c:a", "aac"]);
    }

    let result = command
        .args(["-movflags", "+faststart"])
        .arg(output_path)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| media_command_error("ffmpeg", error))?;

    if !result.status.success() {
        let details = String::from_utf8_lossy(&result.stderr);
        eprintln!("FFmpeg завершился с ошибкой: {details}");
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "Не удалось склеить видео. Проверь, что все файлы MP4 открываются.".into(),
        ));
    }

    Ok(())
}

struct ClipInfo {
    video_stream: u64,
    audio_stream: Option<u64>,
    audio_offset: f64,
    duration: f64,
    display_width: f64,
    display_height: f64,
}

impl ClipInfo {
    fn canvas_size(&self) -> (u32, u32) {
        let scale = (1920.0 / self.display_width.max(self.display_height)).min(1.0);
        let even = |size: f64| (size * scale / 2.0).round().max(1.0) as u32 * 2;
        (even(self.display_width), even(self.display_height))
    }
}

async fn probe_clip(path: &Path, number: usize) -> Result<ClipInfo, RequestError> {
    let result = Command::new("ffprobe")
        .args([
            "-v", "error", "-protocol_whitelist", "file", "-f", "mov", "-show_entries",
            "stream=index,codec_type,width,height,sample_aspect_ratio,duration,start_time:stream_disposition=attached_pic:stream_side_data=rotation:format=duration",
            "-of", "json", "-i",
        ])
        .arg(path)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| media_command_error("ffprobe", error))?;

    let invalid = || {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("Файл №{number} не удалось прочитать как видео MP4."),
        )
    };
    if !result.status.success() {
        eprintln!(
            "Ошибка ffprobe: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        return Err(invalid());
    }
    let info: Value = serde_json::from_slice(&result.stdout).map_err(|_| invalid())?;
    let streams = info["streams"].as_array().ok_or_else(invalid)?;
    let video = streams
        .iter()
        .find(|stream| {
            stream["codec_type"] == "video"
                && stream["disposition"]["attached_pic"].as_u64() != Some(1)
        })
        .ok_or_else(invalid)?;
    let audio = streams
        .iter()
        .find(|stream| stream["codec_type"] == "audio");
    let seconds = |value: &Value| {
        value
            .as_str()?
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
    };
    let duration = seconds(&video["duration"])
        .or_else(|| seconds(&info["format"]["duration"]))
        .filter(|&duration| duration > 0.0)
        .ok_or_else(invalid)?;

    let sar = video["sample_aspect_ratio"]
        .as_str()
        .and_then(|ratio| ratio.split_once(':'))
        .and_then(|(num, den)| {
            Some(num.parse::<u32>().ok()? as f64 / den.parse::<u32>().ok()? as f64)
        })
        .filter(|&ratio| ratio.is_finite() && ratio > 0.0)
        .unwrap_or(1.0);
    let mut display_width = video["width"]
        .as_u64()
        .filter(|&n| n > 0)
        .ok_or_else(invalid)? as f64
        * sar;
    let mut display_height = video["height"]
        .as_u64()
        .filter(|&n| n > 0)
        .ok_or_else(invalid)? as f64;
    if video["side_data_list"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|data| {
            data["rotation"]
                .as_i64()
                .is_some_and(|rotation| rotation.rem_euclid(180) == 90)
        })
    {
        std::mem::swap(&mut display_width, &mut display_height);
    }

    let video_start = seconds(&video["start_time"]).unwrap_or(0.0);
    let audio_start = audio
        .and_then(|stream| seconds(&stream["start_time"]))
        .unwrap_or(video_start);
    Ok(ClipInfo {
        video_stream: video["index"].as_u64().ok_or_else(invalid)?,
        audio_stream: audio.and_then(|stream| stream["index"].as_u64()),
        audio_offset: audio_start - video_start,
        duration,
        display_width,
        display_height,
    })
}

fn media_command_error(program: &str, error: io::Error) -> RequestError {
    eprintln!("Не удалось запустить {program}: {error}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Не удалось запустить {program}. Проверь, что он установлен."),
    )
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
