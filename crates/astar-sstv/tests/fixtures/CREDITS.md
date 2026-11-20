# Fixture credits

All three files come from Wikimedia Commons. Downloads were converted with
ffmpeg and nothing else was changed.

## martin1-sunset-8k.flac

- Commons title: File:SSTV_sunset_audio.ogg (a Martin 1 transmission)
- Author: Mysid (https://commons.wikimedia.org/wiki/User:Mysid)
- Licence: CC BY 2.5 (https://creativecommons.org/licenses/by/2.5/)
- Source: https://upload.wikimedia.org/wikipedia/commons/c/ce/SSTV_sunset_audio.ogg
- Converted: `ffmpeg -i SSTV_sunset_audio.ogg -ac 1 -ar 8000 -sample_fmt s16 martin1-sunset-8k.flac`

## robot36-french-logo-8k.flac

- Commons title: File:French_Wikipedia_logo_in_SSTV.flac (a Robot 36 transmission)
- Author: Wizly-08 (https://commons.wikimedia.org/wiki/User:Wizly-08)
- Licence: CC0 1.0 (https://creativecommons.org/publicdomain/zero/1.0/)
- Source: https://upload.wikimedia.org/wikipedia/commons/2/29/French_Wikipedia_logo_in_SSTV.flac
- Converted: `ffmpeg -i French_Wikipedia_logo_in_SSTV.flac -ac 1 -ar 8000 -sample_fmt s16 robot36-french-logo-8k.flac`

## robot36-french-logo.png

- Commons title: File:French_Wikipedia_logo_in_SSTV.png (the picture that was transmitted)
- Author: Kilyann Le Hen
- Licence: CC0 1.0 (https://creativecommons.org/publicdomain/zero/1.0/)
- Source: https://upload.wikimedia.org/wikipedia/commons/3/3d/French_Wikipedia_logo_in_SSTV.png
- Unmodified.
