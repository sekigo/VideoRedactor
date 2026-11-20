use ffmpeg_rs::{FFmpegBuilder, Input, Output};
use ffmpeg_rs::codec::presets;
use ffmpeg_rs::filter::VideoFilter;
use ffmpeg_common::{Codec, Duration};
use std::{collections::HashMap, env};



fn main(){
    let mut args = HashMap::new();

    for arg in env::args().skip(1){
        let Some((key, value)) = arg.split_once('=') else {
            eprintln!("Ожидался аргумент ключ=значение, получено: {arg}");
                        std::process::exit(1);
        };
        args.insert(key.to_string(), value.to_string())
    };

    let Some(input) = args.get("input") else {
        eprintln!("Укажи input=путь_к_видео");
        std::process::exit(1);
    };

    // let Some(output) = arg.get("output") else {
    //     eprintln!("Укажи output=путь_к_видео");
    //     std::process::exit(1);
    // }



    let Some(left) = args.get("left") else {
        eprintln!("Укажи len=left");
        std::process::exit(1);
    };

    let Some(right) = args.get("right") else {
        eprintln!("Укажи right=правый край");
        std::process::exit(1);
    };

    let video = FFmpegBuilder::new()
        .input(
            Input::new(input)
                .seek(Duration::from_secs(left))
                .duration(Duration::from_secs(right))
        )
        .output(
            Output::new("output.mp4")
                .video_codec_opts(presets::h264::youtube_1080p())
                    .audio_codec(Codec::aac())
                    .metadata("title", "My Video")
                    .faststart()
        )
        .video_filter(VideoFilter::scale(1920,1080))
        .overwrite()
        .on_progress(|p| {
            println!("Progress: {:?}", p);
        })
        .run();

}
