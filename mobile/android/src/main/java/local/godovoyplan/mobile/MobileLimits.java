package local.godovoyplan.mobile;

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;

final class MobileLimits {
    static final int MAX_TITLE_CHARS = 160;
    static final int MAX_DESCRIPTION_CHARS = 4000;
    static final int MAX_NOTE_CHARS = 4000;
    static final int MAX_PREPARATION_DAYS = 366;
    static final int MAX_TASKS = 100;
    static final int MAX_EXCHANGE_BYTES = 9 * 1024 * 1024;

    private MobileLimits() {}

    static int characterCount(String value) {
        return value.codePointCount(0, value.length());
    }

    static boolean withinTitleLimit(String value) {
        return characterCount(value) <= MAX_TITLE_CHARS;
    }

    static boolean withinDescriptionLimit(String value) {
        return characterCount(value) <= MAX_DESCRIPTION_CHARS;
    }

    static boolean withinNoteLimit(String value) {
        return characterCount(value) <= MAX_NOTE_CHARS;
    }

    static boolean withinExchangeLimit(long size) {
        return size >= 0 && size <= MAX_EXCHANGE_BYTES;
    }

    static void copyExchange(InputStream input, OutputStream output) throws IOException {
        if (input == null || output == null) throw new IOException("Провайдер не открыл документ.");
        byte[] buffer = new byte[32 * 1024];
        int count, total = 0;
        while ((count = input.read(buffer)) != -1) {
            if (count > MAX_EXCHANGE_BYTES - total)
                throw new IOException("Размер файла превышает допустимый предел.");
            total += count;
            output.write(buffer, 0, count);
        }
        output.flush();
    }
}
