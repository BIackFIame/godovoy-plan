package local.godovoyplan.mobile;

import android.app.Application;
import android.net.Uri;
import android.os.Handler;
import android.os.Looper;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;
import java.util.UUID;
import java.util.concurrent.Callable;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionException;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.RejectedExecutionException;
import java.util.concurrent.SynchronousQueue;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;
import org.json.JSONException;
import org.json.JSONObject;

public final class CalendarApplication extends Application {
    interface Work {
        Object run() throws Exception;
    }
    interface Reply {
        void accept(Object data, String error);
    }

    private final ExecutorService worker = Executors.newSingleThreadExecutor();
    private final ExecutorService documents = new ThreadPoolExecutor(0, 1, 30, TimeUnit.SECONDS,
            new SynchronousQueue<>());
    private final Handler ui = new Handler(Looper.getMainLooper());
    private long handle;
    private File profile;

    @Override
    public void onCreate() {
        super.onCreate();
        profile = new File(getNoBackupFilesDir(), "profile");
    }

    static JSONObject message(String op, Object... fields) {
        JSONObject object = new JSONObject();
        try {
            object.put("op", op);
            for (int i = 0; i < fields.length; i += 2) {
                object.put((String) fields[i], fields[i + 1] == null ? JSONObject.NULL : fields[i + 1]);
            }
        } catch (JSONException unexpected) {
            throw new IllegalArgumentException(unexpected);
        }
        return object;
    }

    private void open() throws IOException {
        if (handle == 0) {
            handle = Core.open(profile.getAbsolutePath().getBytes(StandardCharsets.UTF_8));
            if (handle == 0) {
                byte[] detail = Core.openError();
                throw new IOException(detail == null ? "Не удалось открыть личную папку календаря."
                                                     : new String(detail, StandardCharsets.UTF_8));
            }
        }
    }

    private Object invoke(JSONObject command) throws Exception {
        open();
        byte[] response = Core.request(handle, command.toString().getBytes(StandardCharsets.UTF_8));
        if (response == null)
            throw new IOException("Не удалось выполнить запрос к календарю.");
        JSONObject result = new JSONObject(new String(response, StandardCharsets.UTF_8));
        if (!result.getBoolean("ok"))
            throw new IOException(result.optString("error", "Не удалось сохранить изменения."));
        return result.opt("data");
    }

    private void queue(ExecutorService executor, Work work, Reply reply) {
        executor.execute(() -> {
            Object result = null;
            String issue = null;
            try {
                result = work.run();
            } catch (Exception ex) {
                issue = ex.getLocalizedMessage() == null ? "Ошибка приложения." : ex.getLocalizedMessage();
            }
            Object data = result;
            String error = issue;
            if (reply != null)
                ui.post(() -> reply.accept(data, error));
        });
    }

    void call(JSONObject command, Reply reply) {
        queue(worker, () -> invoke(command), reply);
    }

    private void transfer(Work work, Reply reply) {
        try {
            queue(documents, work, reply);
        } catch (RejectedExecutionException busy) {
            if (reply != null)
                ui.post(() -> reply.accept(null, "Дождитесь завершения обмена файлом."));
        }
    }

    // Only the document worker waits here. The core worker never waits for provider I/O.
    private <T> T coreStep(Callable<T> step) throws Exception {
        try {
            return CompletableFuture.supplyAsync(() -> {
                try {
                    return step.call();
                } catch (Exception issue) {
                    throw new CompletionException(issue);
                }
            }, worker).join();
        } catch (CompletionException failed) {
            if (failed.getCause() instanceof Exception)
                throw (Exception) failed.getCause();
            throw failed;
        }
    }

    private static final int FILE_LIMIT = 9 * 1024 * 1024;
    private void copy(InputStream in, OutputStream out) throws IOException {
        if (in == null || out == null)
            throw new IOException("Провайдер не открыл документ.");
        byte[] buffer = new byte[32 * 1024];
        int count, total = 0;
        while ((count = in.read(buffer)) != -1) {
            if (count > FILE_LIMIT - total)
                throw new IOException("Размер файла превышает допустимый предел.");
            total += count;
            out.write(buffer, 0, count);
        }
        out.flush();
    }

    private File exchangeFolder() throws IOException {
        File folder = new File(profile, "exchange").getCanonicalFile();
        if (!folder.isDirectory())
            throw new IOException("Не удалось открыть папку обмена.");
        return folder;
    }

    void importDocument(Uri source, Reply reply) {
        transfer(() -> {
            File imported = coreStep(() -> {
                open();
                return new File(exchangeFolder(), UUID.randomUUID() + ".yrplan");
            });
            try {
                try (InputStream in = getContentResolver().openInputStream(source);
                        OutputStream out = new FileOutputStream(imported)) {
                    copy(in, out);
                }
                return coreStep(() -> invoke(message("import", "path", imported.getAbsolutePath())));
            } finally {
                imported.delete();
            }
        }, reply);
    }

    void exportDocument(Uri target, Reply reply) {
        transfer(() -> {
            String path = coreStep(() -> (String) invoke(message("export")));
            File exported = new File(path).getCanonicalFile();
            if (!exchangeFolder().equals(exported.getParentFile())
                    || !exported.getName().endsWith(".yrplan")) {
                throw new IOException("Некорректный файл обмена.");
            }
            try (InputStream in = new FileInputStream(exported);
                    OutputStream out = getContentResolver().openOutputStream(target, "wt")) {
                copy(in, out);
                return null;
            } finally {
                exported.delete();
            }
        }, reply);
    }
}
