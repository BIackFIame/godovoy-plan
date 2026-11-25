package local.godovoyplan.mobile;

import android.content.Context;
import android.graphics.Canvas;
import android.graphics.Paint;
import android.graphics.RectF;
import android.view.MotionEvent;
import android.view.View;

final class YearDial extends View {
    interface Pick {
        void month(int number);
    }
    private final Paint paint = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final int[] counts;
    private final int year, selectedMonth;
    private static final String[] LABELS = {
            "янв", "фев", "мар", "апр", "май", "июн", "июл", "авг", "сен", "окт", "ноя", "дек"};
    private final Pick pick;
    private float cx, cy, radius;

    YearDial(Context context, int year, int[] counts, int selectedMonth, Pick pick) {
        super(context);
        this.year = year;
        this.counts = counts;
        this.selectedMonth = selectedMonth;
        this.pick = pick;
        // The neighbouring month filter provides the same 12 destinations to accessibility tools.
        setImportantForAccessibility(IMPORTANT_FOR_ACCESSIBILITY_NO);
    }
    @Override
    protected void onDraw(Canvas canvas) {
        float d = getResources().getDisplayMetrics().density;
        cx = getWidth() / 2f;
        cy = getHeight() / 2f;
        radius = Math.min(cx, cy) - 13 * d;
        float thickness = 13 * d;
        RectF oval = new RectF(cx - radius, cy - radius, cx + radius, cy + radius);
        paint.setStyle(Paint.Style.STROKE);
        paint.setStrokeWidth(thickness);
        for (int m = 0; m < 12; m++) {
            paint.setColor(m + 1 == selectedMonth ? 0xff173d78
                            : counts[m] == 0      ? 0xffe0e5df
                            : counts[m] < 3       ? 0xff6790cb
                                                  : 0xff2455aa);
            canvas.drawArc(oval, -90 + 30 * m + 1.6f, 26.8f, false, paint);
        }
        paint.setStyle(Paint.Style.FILL);
        paint.setTextAlign(Paint.Align.CENTER);
        paint.setColor(0xff252c27);
        paint.setTextSize(22 * d);
        canvas.drawText(Integer.toString(year), cx, cy + (selectedMonth > 0 ? 2 : 7) * d, paint);
        paint.setTextSize(10 * d);
        if (selectedMonth > 0)
            canvas.drawText("Весь год", cx, cy + 15 * d, paint);
        paint.setColor(0xff3c4941);
        for (int i = 0; i < 12; i++) {
            double angle = Math.toRadians(-75 + i * 30);
            canvas.drawText(LABELS[i], cx + (radius - 15 * d) * (float) Math.cos(angle),
                    cy + (radius - 15 * d) * (float) Math.sin(angle) + 3 * d, paint);
        }
    }
    @Override
    public boolean onTouchEvent(MotionEvent event) {
        if (event.getAction() == MotionEvent.ACTION_UP) {
            double dx = event.getX() - cx, dy = event.getY() - cy, distance = Math.hypot(dx, dy);
            if (distance < radius * .57) {
                if (selectedMonth > 0) {
                    performClick();
                    pick.month(0);
                }
                return true;
            }
            if (distance > radius * 1.25)
                return false;
            int m = (int) ((Math.toDegrees(Math.atan2(dy, dx)) + 450) % 360 / 30) + 1;
            performClick();
            pick.month(m);
        }
        return true;
    }
    @Override
    public boolean performClick() {
        super.performClick();
        return true;
    }
}
